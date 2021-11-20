// Copyright 2021 Ross Light
// Copyright 2010-2018 Avery Pennarun and contributors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
// SPDX-License-Identifier: Apache-2.0

//! Code for parallel-building a set of targets.

use futures::future::FusedFuture;
use futures::stream::{FusedStream, FuturesUnordered, Stream};
use futures::{pin_mut, select};
use nix;
use nix::errno::Errno;
use nix::sys::signal::{self, SigHandler, Signal};
use nix::sys::stat;
use nix::sys::wait;
use nix::unistd::{self, ForkResult, Pid};
use rand;
use rusqlite::{DropBehavior, TransactionBehavior};
use std::borrow::Cow;
use std::cell::{Cell, RefCell};
use std::cmp;
use std::collections::{HashSet, VecDeque};
use std::env;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fs::{self, File, Metadata};
use std::future::{self, Future};
use std::io::{self, BufRead, BufReader};
use std::mem;
use std::os::unix::io::RawFd;
use std::panic;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process;
use std::rc::Rc;
use std::time::Duration;
use tempfile::{self, Builder as TempFileBuilder};
use zombiezen_const_cstr::const_cstr;

use super::cycles;
use super::deps::Dirtiness;
use super::env::{
    Env, OptionalBool, ENV_DEPTH, ENV_LOG, ENV_LOG_INODE, ENV_PWD, ENV_TARGET, ENV_VERBOSE,
    ENV_XTRACE,
};
use super::error::{RedoError, RedoErrorKind};
use super::exits::*;
use super::helpers::{self, OsBytes, RedoPath, RedoPathBuf};
use super::jobserver::JobServerHandle;
use super::logs::{self, LogBuilder};
use super::paths;
use super::state::{self, Lock, LockType, ProcessState, ProcessTransaction, Stamp};

struct BuildJob<'a> {
    /// Original target name. (Not relative to `Env.base`).
    t: RedoPathBuf,
    sf: state::File,
    lock: Lock,
    should_build_func:
        Rc<dyn Fn(&mut ProcessTransaction, &RedoPath) -> Result<(bool, Dirtiness), RedoError> + 'a>,
}

impl BuildJob<'_> {
    /// Actually start running this job in a subproc, if needed.
    ///
    /// `ps_ref` must be the same state as in `ptx`. `ps_ref` is mutably borrowed
    /// during the future's execution.
    fn start<'a>(
        self,
        ps_ref: Rc<RefCell<&'a mut ProcessState>>,
        mut ptx: ProcessTransaction<'_>,
        server: &JobServerHandle,
    ) -> Result<Pin<Box<dyn Future<Output = i32> + 'a>>, RedoError> {
        let before_t = try_stat(self.t.as_path()).map_err(RedoError::opaque_error)?;
        debug_assert!(self.lock.is_owned());
        let (is_target, dirty) = (self.should_build_func)(&mut ptx, &self.t)?;
        match dirty {
            Dirtiness::Clean => {
                // Target doesn't need to be built; skip the whole task.
                if is_target {
                    logs::meta(
                        "unchanged",
                        state::target_relpath(ptx.state().env(), &self.t)?.as_str(),
                        None,
                    );
                }
                Ok(Box::pin(future::ready(EXIT_SUCCESS)))
            }
            Dirtiness::Dirty => self.start_self(ps_ref, ptx, server, before_t),
            Dirtiness::NeedTargets(targets) => {
                if ptx.state().env().no_oob {
                    self.start_self(ps_ref, ptx, server, before_t)
                } else {
                    self.start_deps_unlocked(ptx, server, targets)
                }
            }
        }
    }

    /// Run `JobServer::start` to build this object's target file.
    fn start_self<'a>(
        self,
        ps_ref: Rc<RefCell<&'a mut ProcessState>>,
        mut ptx: ProcessTransaction<'_>,
        server: &JobServerHandle,
        before_t: Option<Metadata>,
    ) -> Result<Pin<Box<dyn Future<Output = i32> + 'a>>, RedoError> {
        use std::os::unix::fs::MetadataExt;
        use std::os::unix::io::AsRawFd;

        debug_assert!(self.lock.is_owned());
        let t = self.t;
        let mut sf = self.sf;
        let lock = self.lock;

        let newstamp = sf.read_stamp(ptx.state().env())?;
        if sf.is_generated()
            && !newstamp.is_missing()
            && (sf.is_override || Stamp::detect_override(sf.stamp.as_ref().unwrap(), &newstamp))
        {
            let nice_t = nice(ptx.state().env(), &t).map_err(RedoError::opaque_error)?;
            state::warn_override(&nice_t);
            if !sf.is_override {
                log_warn!("{:?} - old: {:?}\n", &nice_t, &sf.stamp);
                log_warn!("{:?} - old: {:?}\n", &nice_t, &newstamp);
                sf.set_override(ptx.state().env())?;
            }
            sf.save(&mut ptx)?;
            // Fall through and treat it the same as a static file.
        }
        if Path::new(&t).exists()
            && !Path::new(&t).join(".").is_dir()
            && (sf.is_override || !sf.is_generated())
        {
            // an existing source file that was not generated by us.
            // This step is mentioned by djb in his notes.
            // For example, a rule called default.c.do could be used to try
            // to produce hello.c, but we don't want that to happen if
            // hello.c was created by the end user.
            log_debug2!("-- static ({:?})\n", &t);
            if !sf.is_override {
                sf.set_static(ptx.state().env())?;
            }
            sf.save(&mut ptx)?;
            return Ok(Box::pin(future::ready(EXIT_SUCCESS)));
        }
        sf.zap_deps1(&mut ptx)?;
        let df = match paths::find_do_file(&mut ptx, &mut sf)? {
            Some(df) => df,
            None => {
                let rv = if Path::new(&t).exists() {
                    sf.set_static(ptx.state().env())?;
                    EXIT_SUCCESS
                } else {
                    log_err!("no rule to redo {:?}\n", &t);
                    sf.set_failed(ptx.state().env())?;
                    EXIT_FAILURE
                };
                sf.save(&mut ptx)?;
                return Ok(Box::pin(future::ready(rv)));
            }
        };
        // There is no good place for us to pre-create a temp file for
        // stdout.  The target dir might not exist yet, or it might currently
        // exist but get wiped by the .do script.  Other dirs, like the one
        // containing the .do file, might be mounted readonly.  We can put it
        // in the system temp dir, but then we can't necessarily rename it to
        // the target filename because it might cross filesystem boundaries.
        // Also, if redo is interrupted, it would leave a temp file lying
        // around.  To avoid all this, use mkstemp() to create a temp file
        // wherever it wants to, and immediately unlink it, but keep a file
        // handle open.  When the .do script finishes, we can copy the
        // content out of that nameless file handle into a file in the same
        // dir as the target (which by definition must now exist, if you
        // wanted the target to exist).
        //
        // On the other hand, the $3 temp filename can be hardcoded to be in
        // the target directory, even if that directory does not exist.
        // It's not *redo*'s job to create that file.  The .do file will
        // create it, if it wants, and it's the .do file's job to first ensure
        // that the directory exists.
        let tmp_name = {
            let mut tmp_base_name = OsString::new();
            tmp_base_name.push(&df.base_name);
            tmp_base_name.push(&df.ext);
            tmp_base_name.push(".redo.tmp");
            df.do_dir.join(tmp_base_name)
        };
        helpers::unlink(&tmp_name).map_err(RedoError::opaque_error)?;
        let out_file = tempfile::tempfile().map_err(RedoError::opaque_error)?;
        helpers::close_on_exec(out_file.as_raw_fd(), true).map_err(RedoError::opaque_error)?;
        // this will run in the dofile's directory, so use only basenames here
        let arg1 = {
            // target name (with extension)
            let mut arg1 = OsString::new();
            arg1.push(&df.base_name);
            arg1.push(&df.ext);
            arg1
        };
        let arg2 = {
            // target name (without extension)
            let mut arg2 = OsString::new();
            arg2.push(&df.base_name);
            arg2
        };
        let cwd = env::current_dir().map_err(RedoError::opaque_error)?;
        let mut argv: Vec<OsString> = vec![
            OsString::from("sh"),
            OsString::from("-e"),
            df.do_file.clone(),
            arg1,
            arg2,
            // $3 temp output file name
            state::relpath(helpers::abs_path(&cwd, &tmp_name), &df.do_dir)
                .map_err(RedoError::opaque_error)?
                .into_os_string(),
        ];
        if ptx.state().env().verbose != 0 {
            argv[1].push("v");
        }
        if ptx.state().env().xtrace != 0 {
            argv[1].push("x");
        }
        let firstline = {
            let f = File::open(df.do_dir.join(&df.do_file)).map_err(RedoError::opaque_error)?;
            let mut f = BufReader::new(f);
            let mut firstline = String::new();
            f.read_line(&mut firstline)
                .map_err(RedoError::opaque_error)?;
            firstline
        };
        let firstline = firstline.trim();
        if firstline.starts_with("#!/") {
            let interpreter: Vec<&str> = firstline[2..].split(' ').collect();
            let mut new_argv: Vec<OsString> =
                Vec::with_capacity(argv.len() - 2 + interpreter.len());
            new_argv.extend(interpreter.into_iter().map(|s| OsString::from(s)));
            new_argv.extend(argv.into_iter().skip(2));
            argv = new_argv;
        }
        // make sure to create the logfile *before* writing the meta() about
        // it.  that way redo-log won't trace into an obsolete logfile.
        //
        // We open a temp file and atomically rename it into place here.
        // This guarantees that redo-log will never experience a file that
        // gets truncated halfway through reading (eg.  if we build the same
        // target more than once in a run).  Similarly, we don't want to
        // actually unlink() the file in case redo-log is about to start
        // reading a previous instance created during this session.  It
        // should always see either the old or new instance.
        if ptx.state().env().log().unwrap_or(true) {
            let lfend = state::logname(ptx.state().env(), sf.id());
            // Make sure the temp file is in the same directory as lfend,
            // so we can be sure of our ability to rename it atomically later.
            let lfd = TempFileBuilder::new()
                .prefix("redo.")
                .suffix(".log.tmp")
                .tempfile_in(lfend.parent().unwrap())
                .map_err(RedoError::opaque_error)?;
            lfd.persist(lfend).map_err(RedoError::opaque_error)?;
        }
        let mut dof = state::File::from_name(&mut ptx, &df.do_dir.join(&df.do_file), true)?;
        dof.set_static(ptx.state().env())?;
        dof.save(&mut ptx)?;
        let ps = ptx.commit().map_err(RedoError::opaque_error)?;
        logs::meta("do", state::target_relpath(ps.env(), &t)?.as_str(), None);

        // Wrap out_file in a Cell, since we drop it in the subprocess.
        // Rust can't tell that the closure is not called in the parent process.
        let out_file = Cell::new(Some(out_file));

        let job = server.start(t.as_str().to_string(), || {
            // TODO(someday): Log errors.
            use std::iter::FromIterator;

            // careful: REDO_PWD was the PWD relative to the STARTPATH at the time
            // we *started* building the current target; but that target ran
            // redo-ifchange, and it might have done it from a different directory
            // than we started it in.  So os.getcwd() might be != REDO_PWD right
            // now.
            assert!(ps.is_flushed());
            let newp = match df.do_dir.canonicalize() {
                Ok(newp) => newp,
                Err(_) => return EXIT_FAILURE,
            };
            // CDPATH apparently caused unexpected 'cd' output on some platforms.
            env::remove_var("CDPATH");
            env::set_var(
                ENV_PWD,
                match state::relpath(newp, &ps.env().startdir) {
                    Ok(path) => path,
                    Err(_) => return EXIT_FAILURE,
                },
            );
            env::set_var(ENV_TARGET, {
                let mut target = OsString::new();
                target.push(&df.base_name);
                target.push(&df.ext);
                target
            });
            env::set_var(ENV_DEPTH, {
                let mut depth = String::new();
                depth.push_str(ps.env().depth());
                depth.push_str("  ");
                depth
            });
            if ps.env().xtrace == 1 {
                env::set_var(ENV_XTRACE, "0");
            }
            if ps.env().verbose == 1 {
                env::set_var(ENV_VERBOSE, "0");
            }
            cycles::add(lock.file_id().to_string());
            if !df.do_dir.as_os_str().is_empty() {
                if env::set_current_dir(&df.do_dir).is_err() {
                    return EXIT_FAILURE;
                }
            }
            let out_file = out_file.take().unwrap();
            if unistd::dup2(out_file.as_raw_fd(), 1).is_err() {
                return EXIT_FAILURE;
            }
            mem::drop(out_file);
            if helpers::close_on_exec(1, false).is_err() {
                return EXIT_FAILURE;
            }
            if ps.env().log().unwrap_or(true) {
                let cur_inode = stat::fstat(2)
                    .map(|st| OsString::from(st.st_ino.to_string()))
                    .unwrap_or_default();
                if ps.env().log_inode().is_empty() || ps.env().log_inode() == cur_inode {
                    // .do script has *not* redirected stderr, which means we're
                    // using redo-log's log saving mode.  That means subprocs
                    // should be logged to their own file.  If the .do script
                    // *does* redirect stderr, that redirection should be inherited
                    // by subprocs, so we'd do nothing.
                    let logf = match File::create(state::logname(ps.env(), sf.id())) {
                        Ok(logf) => logf,
                        Err(e) => {
                            eprintln!("create log: {}", e);
                            return EXIT_FAILURE;
                        }
                    };
                    let new_inode = logf
                        .metadata()
                        .map(|m| OsString::from(m.ino().to_string()))
                        .unwrap_or_default();
                    env::set_var(ENV_LOG, "1"); // .do files can check this
                    env::set_var(ENV_LOG_INODE, new_inode);
                    unistd::dup2(logf.as_raw_fd(), 2).expect("cannot redirect log to stderr");
                    let _ = helpers::close_on_exec(2, false);
                }
            } else {
                env::remove_var(ENV_LOG_INODE);
                env::set_var(ENV_LOG, "0");
            }
            if unsafe { signal::signal(Signal::SIGPIPE, SigHandler::SigDfl) }.is_err() {
                return EXIT_FAILURE;
            }
            if ps.env().verbose != 0 || ps.env().xtrace != 0 {
                let mut s = String::new();
                s.push_str("* ");
                s.push_str(&argv[0].to_str().unwrap().replace("\n", " "));
                for a in &argv[1..] {
                    s.push(' ');
                    s.push_str(&a.to_str().unwrap().replace("\n", " "));
                }
                logs::write(&s);
            }
            let argv = Vec::from_iter(
                argv.iter()
                    .map(|s| CString::new(Vec::from_iter(OsBytes::new(s))).unwrap()),
            );
            let _ = unistd::execvp(argv[0].as_c_str(), argv.as_slice());
            // Returns only if execvp failed.
            EXIT_FAILURE
        })?;
        let out_file = out_file.take().unwrap();
        Ok(Box::pin(async move {
            let _lock = lock; // ensure we hold the lock until after state has been recorded
            let mut rv = job.await;
            let mut ps = ps_ref.borrow_mut();
            let mut ptx = match ProcessTransaction::new(*ps, TransactionBehavior::Immediate) {
                Ok(ptx) => ptx,
                Err(_) => return EXIT_BUILD_JOB_ERROR,
            };
            rv = BuildJob::record_new_state(
                &mut ptx, &t, sf, &before_t, out_file, &tmp_name, &argv, rv,
            );
            if let Err(e) = ptx.commit() {
                eprintln!("{:?}: {}", &t, e);
                return EXIT_BUILD_JOB_ERROR;
            }
            rv
        }))
    }

    /// Run `server.start` to build objects needed to check deps.
    ///
    /// Out-of-band redo of some sub-objects.  This happens when we're not
    /// quite sure if t needs to be built or not (because some children
    /// look dirty, but might turn out to be clean thanks to redo-stamp
    /// checksums).  We have to call redo-unlocked to figure it all out.
    ///
    /// Note: redo-unlocked will handle all the updating of sf, so we don't
    /// have to do it here, nor call _record_new_state.  However, we have to
    /// hold onto the lock because otherwise we would introduce a race
    /// condition; that's why it's called redo-unlocked, because it doesn't
    /// grab a lock.
    fn start_deps_unlocked<'a>(
        self,
        ptx: ProcessTransaction<'_>,
        server: &JobServerHandle,
        targets: Vec<state::File>,
    ) -> Result<Pin<Box<dyn Future<Output = i32> + 'a>>, RedoError> {
        use std::iter::FromIterator;

        // FIXME: redo-unlocked is kind of a weird hack.
        //  Maybe we should just start jobs to build the necessary deps
        //  directly from this process, and when done, reconsider building
        //  the target we started with.  But that makes this one process's
        //  build recursive, where currently it's flat.
        let here = env::current_dir().map_err(RedoError::opaque_error)?;
        let fix = |p: &RedoPath| -> io::Result<CString> {
            state::relpath(ptx.state().env().base().join(p), &here)
                .map(|p| CString::new(Vec::from_iter(OsBytes::new(&p))).unwrap())
        };
        let mut argv: Vec<Cow<CStr>> = vec![
            Cow::Borrowed(const_cstr!("redo-unlocked").as_cstr()),
            Cow::Owned(fix(self.sf.name()).map_err(RedoError::opaque_error)?),
        ];
        {
            let mut names: HashSet<CString> = HashSet::new();
            for d in targets {
                names.insert(fix(d.name()).map_err(RedoError::opaque_error)?);
            }
            argv.extend(names.drain().map(|s| Cow::Owned(s)));
        }
        logs::meta(
            "check",
            state::target_relpath(ptx.state().env(), &self.t)?.as_str(),
            None,
        );
        let state = ptx.commit().map_err(RedoError::opaque_error)?;
        let job = server.start(self.t.into_string(), || {
            env::set_var(ENV_DEPTH, {
                let mut depth = state.env().depth().to_string();
                depth.push_str("  ");
                depth
            });
            if unsafe { signal::signal(Signal::SIGPIPE, SigHandler::SigDfl) }.is_err() {
                return EXIT_FAILURE;
            }
            let _ = unistd::execvp(&argv[0], argv.as_slice());
            // Returns only if execvp failed.
            eprintln!("Failed to exec: {:?}", argv);
            EXIT_FAILURE
        })?;
        let lock = self.lock;
        Ok(Box::pin(async move {
            let _lock = lock; // ensure we hold the lock until after the job has finished
            let rv = job.await;
            rv
        }))
    }

    /// After a subtask finishes, handle its changes to the output file.
    //
    /// This is run in the *parent* process.
    //
    /// This includes renaming temp files into place and detecting mistakes
    /// (like writing directly to $1 instead of $3).  We also have to record
    /// the new file stamp data for the completed target.
    fn record_new_state<A: AsRef<OsStr>>(
        ptx: &mut ProcessTransaction<'_>,
        t: &RedoPath,
        mut sf: state::File,
        before_t: &Option<Metadata>,
        mut out_file: File,
        tmp_name: &Path,
        argv: &[A],
        mut rv: i32,
    ) -> i32 {
        use std::io::{Seek, SeekFrom};
        use std::os::unix::fs::MetadataExt;

        let after_t = try_stat(t).expect("cannot get target metadata");
        let st1 = out_file.metadata().expect("cannot get out_file metadata");
        let mut st2 = try_stat(tmp_name).expect("unexpected error when statting $3");
        let modified = match after_t {
            Some(after_t) => {
                !after_t.is_dir()
                    && match before_t {
                        Some(before_t) => match (before_t.modified(), after_t.modified()) {
                            (Ok(before_mod), Ok(after_mod)) => before_mod != after_mod,
                            _ => false,
                        },
                        None => true,
                    }
            }
            None => false,
        };
        if modified {
            eprintln!("{:?} modified {} directly!", argv[2].as_ref(), t);
            eprintln!("... you should update $3 (a temp file) or stdout, not $1.");
            rv = EXIT_TARGET_DIRECTLY_MODIFIED;
        } else if st2.is_some() && st1.size() > 0 {
            eprintln!("{:?} wrote to stdout *and* created $3.", argv[2].as_ref());
            eprintln!("... you should write status messages to stderr, not stdout.");
            rv = EXIT_MULTIPLE_OUTPUTS;
        }
        if rv == EXIT_SUCCESS {
            // FIXME: race condition here between updating stamp/is_generated
            // and actually renaming the files into place.  There needs to
            // be some kind of two-stage commit, I guess.
            if st1.size() > 0 && st2.is_none() {
                // script wrote to stdout.  Copy its contents to the tmpfile.
                helpers::unlink(tmp_name)
                    .expect("failed to remove old temp file before copying stdout");
                match File::create(tmp_name) {
                    Err(e) => {
                        let cwd = &env::current_dir().expect("cannot get working directory");
                        let abs_t = helpers::abs_path(cwd, t);
                        let dnt = abs_t.parent();
                        if dnt.map_or(false, |dnt| dnt.exists()) {
                            // This could happen, so report a simple error message
                            // that gives a hint for how to fix your .do script.
                            log_err!(
                                "{:?}: target dir {:?} does not exist!",
                                t,
                                dnt.unwrap_or(Path::new(""))
                            );
                        } else {
                            // This could happen for, eg. a permissions error on
                            // the target directory.
                            log_err!("{:?}: copy stdout: {}", t, e);
                        }
                        rv = EXIT_BUILD_JOB_ERROR;
                    }
                    Ok(mut newf) => {
                        out_file
                            .seek(SeekFrom::Start(0))
                            .expect("could not seek to beginning of stdout");
                        io::copy(&mut out_file, &mut newf).expect("could not copy stdout");
                        st2 = Some(
                            newf.metadata()
                                .expect("cannot get copied stdout file metadata"),
                        );
                    }
                }
            }
            if st2.is_some() {
                // either $3 file was created *or* stdout was written to.
                // therefore tmpfile now exists.
                if let Err(e) = fs::rename(tmp_name, t) {
                    // This could happen for, eg. a permissions error on
                    // the target directory.
                    log_err!("{:?}: rename {:?}: {}", t, tmp_name, e);
                    rv = EXIT_BUILD_JOB_ERROR;
                }
            } else {
                // no output generated at all; that's ok

                // TODO(maybe): Remove EISDIR/EPERM exception or remove directory?
                // Needed for makedir2 test. :(
                match helpers::unlink(t) {
                    Ok(_)
                    | Err(nix::Error::Sys(Errno::EISDIR))
                    | Err(nix::Error::Sys(Errno::EPERM)) => {}
                    e @ Err(_) => e.expect("failed to remove target file"),
                }
            }
            if let Err(e) = sf.refresh(ptx) {
                log_err!("{:?}: refresh: {}", t, e);
                rv = EXIT_BUILD_JOB_ERROR;
            }
            sf.is_generated = true;
            sf.is_override = false;
            if sf.is_checked(ptx.state().env()) || sf.is_changed(ptx.state().env()) {
                // it got checked during the run; someone ran redo-stamp.
                // update_stamp would call set_changed(); we don't want that,
                // so only use read_stamp.
                sf.stamp = Some(
                    sf.read_stamp(ptx.state().env())
                        .expect("target file stat failed"),
                );
            } else {
                sf.set_checksum(String::new());
                if let Err(e) = sf.update_stamp(ptx.state().env(), false) {
                    log_err!("{:?}: update stamp: {}", t, e);
                    rv = EXIT_BUILD_JOB_ERROR;
                }
                sf.set_changed(ptx.state().env());
            }
        }
        // rv might have changed up above
        if rv != EXIT_SUCCESS {
            helpers::unlink(tmp_name).expect("failed to remove temporary output file");
            if let Err(e) = sf.set_failed(ptx.state().env()) {
                log_err!("{:?}: set failed: {}", t, e);
                rv = EXIT_BUILD_JOB_ERROR;
            }
        }
        if let Err(e) = sf.zap_deps2(ptx) {
            log_err!("{:?}: zap_deps2: {}", t, e);
            rv = EXIT_BUILD_JOB_ERROR;
        }
        if let Err(e) = sf.save(ptx) {
            log_err!("{:?}: set failed: {}", t, e);
            rv = EXIT_BUILD_JOB_ERROR;
        }
        logs::meta(
            "done",
            &format!(
                "{} {}",
                rv,
                state::target_relpath(ptx.state().env(), &t)
                    .expect("cannot format target as relative path")
                    .as_str()
            ),
            None,
        );
        rv
    }
}

/// Build the given list of targets, if necessary.
///
/// Builds in parallel using whatever [`JobServerHandle`] tokens can be obtained.
///
/// `should_build_func` is a callback which determines whether the given target
/// needs to be built, as of the time it is called. The first return value
/// indicates whether the target is a generated file and the second is the
/// dirtiness.
pub async fn run<P, F, E>(
    ps: &mut ProcessState,
    server: &JobServerHandle,
    targets: &[P],
    should_build_func: F,
) -> Result<(), RedoError>
where
    P: AsRef<RedoPath>,
    F: Fn(&mut ProcessTransaction, &RedoPath) -> Result<(bool, Dirtiness), E>,
    E: std::error::Error + Send + Sync + 'static,
{
    use futures::future::FutureExt;
    use futures::stream::StreamExt;
    use rand::seq::SliceRandom;
    use std::convert::TryInto;
    use std::iter::FromIterator;

    let mut target_order = Vec::from_iter(0..targets.len());
    if ps.env().shuffle {
        target_order.shuffle(&mut rand::thread_rng());
    }

    let should_build_func = move |ptx: &mut ProcessTransaction, path: &RedoPath| {
        should_build_func(ptx, path).map_err(|e| {
            let msg = e.to_string();
            RedoError::wrap(e, msg)
        })
    };
    let should_build_func = Rc::new(should_build_func);
    let mut me: Option<(PathBuf, state::File, Lock)> =
        if !ps.env().target().as_os_str().is_empty() && !ps.env().unlocked {
            let mut me = PathBuf::from(&ps.env().startdir);
            me.push(&ps.env().pwd);
            me.push(ps.env().target());
            let myfile = {
                let mut ptx = ProcessTransaction::new(ps, TransactionBehavior::Immediate)
                    .map_err(RedoError::opaque_error)?;
                ptx.set_drop_behavior(DropBehavior::Commit);
                state::File::from_name(&mut ptx, &me, true)?
            };
            let selflock = ps.new_lock(state::LOG_LOCK_MAGIC + myfile.id());
            Some((me, myfile, selflock))
        } else {
            None
        };

    let result: Cell<Result<(), RedoError>> = Cell::new(Ok(()));
    let mut locked: VecDeque<(i64, &RedoPath)> = VecDeque::new();
    let mut cheat = || -> Result<i32, RedoError> {
        let selflock = match &mut me {
            Some((_, _, ref mut selflock)) => selflock,
            None => return Ok(0),
        };
        selflock.try_lock()?;
        if !selflock.is_owned() {
            // redo-log already owns it: let's cheat.
            // Give ourselves one extra token so that the "foreground" log
            // can always make progress.
            Ok(1)
        } else {
            // redo-log isn't watching us (yet)
            selflock.unlock()?;
            Ok(0)
        }
    };
    // In the first cycle, we just build as much as we can without worrying
    // about any lock contention.  If someone else has it locked, we move on.
    let ps_ref = Rc::new(RefCell::new(ps));
    let job_futures: FuturesUnordered<Pin<Box<dyn Future<Output = ()>>>> = FuturesUnordered::new();
    pin_mut!(job_futures);
    {
        let mut seen: HashSet<RedoPathBuf> = HashSet::new();
        for i in target_order.iter().copied() {
            let t = targets[i].as_ref();
            if t.is_empty() {
                log_err!("cannot build the empty target (\"\").\n");
                result.set(Err(RedoErrorKind::InvalidTarget(t.into()).into()));
                break;
            }
            assert!(ps_ref.borrow().is_flushed());
            if seen.contains(t) {
                continue;
            }
            seen.insert(t.into());
            // TODO(maybe): Commit state if !has_token.
            let token_future = server.ensure_token_or_cheat(t.as_str(), &mut cheat).fuse();
            pin_mut!(token_future);
            wait_for(token_future, job_futures.as_mut())
                .await
                .map_err(RedoError::opaque_error)?;
            let errored = {
                let r = result.replace(Ok(()));
                let errored = r.is_err();
                result.set(r);
                errored
            };
            if errored && !ps_ref.borrow().env().keep_going {
                break;
            }
            // TODO(soon): state.check_sane.
            {
                let mut ps = ps_ref.borrow_mut();
                let mut ptx = ProcessTransaction::new(*ps, TransactionBehavior::Immediate)
                    .map_err(RedoError::opaque_error)?;
                ptx.set_drop_behavior(DropBehavior::Commit);
                let mut f = state::File::from_name(&mut ptx, t, true)?;
                let mut lock = ptx.state().new_lock(f.id().try_into().unwrap());
                if ptx.state().env().unlocked {
                    lock.force_owned();
                } else {
                    lock.try_lock()?;
                }
                if !lock.is_owned() {
                    logs::meta(
                        "locked",
                        state::target_relpath(ptx.state().env(), &t)?.as_str(),
                        None,
                    );
                    locked.push_back((f.id(), t));
                } else {
                    // We had to create f before we had a lock, because we need f.id
                    // to make the lock.  But someone may have updated the state
                    // between then and now.
                    // FIXME: separate obtaining the fid from creating the File.
                    // FIXME: maybe integrate locking into the File object?
                    f.refresh(&mut ptx)?;
                    let job = BuildJob {
                        t: t.into(),
                        sf: f,
                        lock,
                        should_build_func: should_build_func.clone(),
                    }
                    .start(ps_ref.clone(), ptx, server)?;
                    let t = t.to_string();
                    let result = &result;
                    job_futures.push(Box::pin(async move {
                        let rv = job.await;
                        if rv != EXIT_SUCCESS {
                            result.set(Err(RedoError::new(format!("{:?}: exit code {}", t, rv))));
                        }
                    }));
                }
            }
            assert!(ps_ref.borrow().is_flushed());
        }
    }

    // Now we've built all the "easy" ones.  Go back and just wait on the
    // remaining ones one by one.  There's no reason to do it any more
    // efficiently, because if these targets were previously locked, that
    // means someone else was building them; thus, we probably won't need to
    // do anything.  The only exception is if we're invoked as redo instead
    // of redo-ifchange; then we have to redo it even if someone else already
    // did.  But that should be rare.
    while !locked.is_empty() || server.is_running() {
        let jobs_done_future = server.wait_all();
        pin_mut!(jobs_done_future);
        wait_for(jobs_done_future, job_futures.as_mut()).await?;
        let errored = {
            let r = result.replace(Ok(()));
            let errored = r.is_err();
            result.set(r);
            errored
        };
        if errored && !ps_ref.borrow().env().keep_going {
            break;
        }
        if let Some((fid, t)) = locked.pop_front() {
            // TODO(soon): check_sane
            let mut lock = ps_ref.borrow().new_lock(fid);
            let mut backoff = Duration::from_millis(100);
            lock.try_lock()?;
            while !lock.is_owned() {
                // Don't spin with 100% CPU while we fight for the lock.
                server
                    .sleep(Duration::from_millis(
                        (rand::random::<f32>()
                            * (cmp::min(backoff, Duration::from_millis(1000)).as_millis()) as f32)
                            as u64,
                    ))
                    .await;
                backoff *= 2;
                // after printing this line, redo-log will recurse into t,
                // whether it's us building it, or someone else.
                logs::meta(
                    "waiting",
                    state::target_relpath(ps_ref.borrow().env(), &t)?.as_str(),
                    None,
                );
                lock.check()?;
                // this sequence looks a little silly, but the idea is to
                // give up our personal token while we wait for the lock to
                // be released; but we should never run ensure_token() while
                // holding a lock, or we could cause deadlocks.
                server.release_mine()?;
                lock.wait_lock(LockType::Exclusive)?;
                // now t is definitely free, so we get to decide whether
                // to build it.
                lock.unlock()?;
                server.ensure_token_or_cheat(t.as_str(), &mut cheat).await?;
                lock.try_lock()?;
            }
            logs::meta(
                "unlocked",
                state::target_relpath(ps_ref.borrow().env(), &t)?.as_str(),
                None,
            );
            {
                let mut ps = ps_ref.borrow_mut();
                let mut ptx = ProcessTransaction::new(*ps, TransactionBehavior::Immediate)
                    .map_err(RedoError::opaque_error)?;
                ptx.set_drop_behavior(DropBehavior::Commit);
                let file = state::File::from_name(&mut ptx, t, true)?;
                if file.is_failed(ptx.state().env()) {
                    result.set(Err(RedoErrorKind::FailedInAnotherThread {
                        target: t.to_redo_path_buf(),
                    }
                    .into()));
                    lock.unlock()?;
                } else {
                    let sf = state::File::from_id(&mut ptx, fid)?;
                    let job = BuildJob {
                        t: t.to_redo_path_buf(),
                        sf,
                        lock,
                        should_build_func: should_build_func.clone(),
                    }
                    .start(ps_ref.clone(), ptx, server)?;
                    let t = t.to_string();
                    let result = &result;
                    job_futures.push(Box::pin(async move {
                        let rv = job.await;
                        if rv != EXIT_SUCCESS {
                            result.set(Err(RedoError::new(format!("{:?}: exit code {}", t, rv))));
                        }
                    }));
                }
            }
        }
    }
    // TODO(maybe): Use !job_futures.is_empty() instead of server.is_running() in
    // the above loop.
    job_futures.fold((), |_, _| future::ready(())).await;
    result.replace(Ok(()))
}

/// Polls a future and a stream, discarding any results from the stream.
async fn wait_for<T, F, S>(mut fg_future: Pin<&mut F>, mut bg_stream: Pin<&mut S>) -> T
where
    F: Future<Output = T> + FusedFuture,
    S: Stream + FusedStream,
{
    use futures::stream::StreamExt;

    let mut next_bg = bg_stream.next();
    loop {
        select! {
            x = fg_future => return x,
            _ = next_bg => {
                next_bg = bg_stream.next();
            }
        }
    }
}

fn try_stat<P: AsRef<Path>>(path: P) -> io::Result<Option<Metadata>> {
    match path.as_ref().symlink_metadata() {
        Ok(m) => Ok(Some(m)),
        Err(e) => match e.kind() {
            io::ErrorKind::NotFound => Ok(None),
            _ => Err(e),
        },
    }
}

pub fn close_stdin() -> Result<(), RedoError> {
    use std::os::unix::io::AsRawFd;
    let f = File::open("/dev/null").map_err(RedoError::opaque_error)?;
    unistd::dup2(f.as_raw_fd(), 0).map_err(RedoError::opaque_error)?;
    Ok(())
}

/// A builder used to start a redo-log instance.
#[derive(Clone, Debug)]
pub struct StdinLogReaderBuilder {
    status: bool,
    details: bool,
    pretty: bool,
    color: OptionalBool,
    debug_locks: bool,
    debug_pids: bool,
}

impl StdinLogReaderBuilder {
    #[inline]
    pub const fn new() -> StdinLogReaderBuilder {
        StdinLogReaderBuilder {
            status: true,
            details: true,
            pretty: true,
            color: OptionalBool::Auto,
            debug_locks: false,
            debug_pids: false,
        }
    }

    /// Set whether to display build summary line at the bottom of the screen.
    #[inline]
    pub fn set_status(&mut self, val: bool) -> &mut Self {
        self.status = val;
        self
    }

    #[inline]
    pub fn set_details(&mut self, val: bool) -> &mut Self {
        self.details = val;
        self
    }

    #[inline]
    pub fn set_pretty(&mut self, val: bool) -> &mut Self {
        self.pretty = val;
        self
    }

    /// Force logs to display without terminal colors.
    #[inline]
    pub fn disable_color(&mut self) -> &mut Self {
        self.color = OptionalBool::Off;
        self
    }

    /// Force logs to display with terminal colors.
    #[inline]
    pub fn force_color(&mut self) -> &mut Self {
        self.color = OptionalBool::On;
        self
    }

    /// Set whether to print messages about file locking.
    #[inline]
    pub fn set_debug_locks(&mut self, val: bool) -> &mut Self {
        self.debug_locks = val;
        self
    }

    /// Set whether to print process IDs as part of log messages.
    #[inline]
    pub fn set_debug_pids(&mut self, val: bool) -> &mut Self {
        self.debug_pids = val;
        self
    }

    // Redirect stderr to a redo-log instance with the given options.
    //
    // Then we automatically run [`logs::setup`] to send the right data format
    // to that redo-log instance.
    pub fn start(&self, e: &Env) -> Result<StdinLogReader, RedoError> {
        use std::io::Write;

        let (r, w) = unistd::pipe().map_err(RedoError::opaque_error)?; // main pipe to redo-log
        let (ar, aw) = unistd::pipe().map_err(RedoError::opaque_error)?; // ack pipe from redo-log --ack-fd
        io::stdout().flush().map_err(RedoError::opaque_error)?;
        io::stderr().flush().map_err(RedoError::opaque_error)?;
        match unsafe { unistd::fork() }.map_err(RedoError::opaque_error)? {
            ForkResult::Parent { child: pid } => {
                let stderr_fd = unistd::dup(2).map_err(RedoError::opaque_error)?; // save for after the log pipe gets closed
                unistd::close(r).map_err(RedoError::opaque_error)?;
                unistd::close(aw).map_err(RedoError::opaque_error)?;
                let mut b = [0u8; 8];
                let bn = unistd::read(ar, &mut b).map_err(RedoError::opaque_error)?;
                let b = &b[..bn];
                if b.is_empty() {
                    // subprocess died without sending us anything: that's bad.
                    log_err!("failed to start redo-log subprocess; cannot continue.\n");
                    process::exit(EXIT_HELPER_FAILURE);
                }
                assert_eq!(b, b"REDO-OK\n");
                // now we know the subproc is running and will report our errors
                // to stderr, so it's okay to lose our own stderr.
                unistd::close(ar).map_err(RedoError::opaque_error)?;
                unistd::dup2(w, 1).map_err(RedoError::opaque_error)?;
                unistd::dup2(w, 2).map_err(RedoError::opaque_error)?;
                unistd::close(w).map_err(RedoError::opaque_error)?;
                LogBuilder::from(e)
                    .parent_logs(true)
                    .pretty(false)
                    .disable_color()
                    .setup(io::stderr());
                Ok(StdinLogReader { pid, stderr_fd })
            }
            ForkResult::Child => {
                let res = panic::catch_unwind(|| -> () {
                    unistd::close(ar).expect("could not close ar");
                    unistd::close(w).expect("could not close w");
                    unistd::dup2(r, 0).expect("could not duplicate pipe to redo-log stdin");
                    unistd::close(r).expect("could not close r");
                    // redo-log sends to stdout (because if you ask for logs, that's
                    // the output you wanted!).  But redo itself sends logs to stderr
                    // (because they're incidental to the thing you asked for).
                    // To make these semantics work, we point redo-log's stdout at
                    // our stderr when we launch it.
                    unistd::dup2(2, 1).expect("could not point stdout to stderr");
                    let mut argv: Vec<Cow<CStr>> = vec![
                        Cow::Borrowed(const_cstr!("redo-log").as_cstr()),
                        Cow::Borrowed(const_cstr!("--recursive").as_cstr()),
                        Cow::Borrowed(const_cstr!("--follow").as_cstr()),
                        Cow::Borrowed(const_cstr!("--ack-fd").as_cstr()),
                        Cow::Owned(CString::new(format!("{}", aw)).unwrap()),
                        if self.status && unistd::isatty(2).unwrap_or(false) {
                            Cow::Borrowed(const_cstr!("--status").as_cstr())
                        } else {
                            Cow::Borrowed(const_cstr!("--no-status").as_cstr())
                        },
                        if self.details {
                            Cow::Borrowed(const_cstr!("--details").as_cstr())
                        } else {
                            Cow::Borrowed(const_cstr!("--no-details").as_cstr())
                        },
                        if self.pretty {
                            Cow::Borrowed(const_cstr!("--pretty").as_cstr())
                        } else {
                            Cow::Borrowed(const_cstr!("--no-pretty").as_cstr())
                        },
                        if self.debug_locks {
                            Cow::Borrowed(const_cstr!("--debug-locks").as_cstr())
                        } else {
                            Cow::Borrowed(const_cstr!("--no-debug-locks").as_cstr())
                        },
                        if self.debug_pids {
                            Cow::Borrowed(const_cstr!("--debug-pids").as_cstr())
                        } else {
                            Cow::Borrowed(const_cstr!("--no-debug-pids").as_cstr())
                        },
                    ];
                    if let Some(color) = self.color.into() {
                        argv.push(if color {
                            Cow::Borrowed(const_cstr!("--color").as_cstr())
                        } else {
                            Cow::Borrowed(const_cstr!("--no-color").as_cstr())
                        });
                    }
                    argv.push(Cow::Borrowed(const_cstr!("-").as_cstr()));
                    if let Err(e) = unistd::execvp(&argv[0], &argv) {
                        eprintln!("redo-log: exec: {}", e);
                    }
                });
                if let Err(e) = res {
                    eprintln!("redo-log: exec: {:?}", e);
                }
                process::exit(EXIT_HELPER_FAILURE);
            }
        }
    }
}

impl Default for StdinLogReaderBuilder {
    #[inline]
    fn default() -> StdinLogReaderBuilder {
        StdinLogReaderBuilder::new()
    }
}

impl From<&Env> for StdinLogReaderBuilder {
    fn from(e: &Env) -> StdinLogReaderBuilder {
        StdinLogReaderBuilder {
            pretty: e.pretty().unwrap_or(true),
            color: e.color(),
            debug_locks: e.debug_locks(),
            debug_pids: e.debug_pids(),
            ..StdinLogReaderBuilder::default()
        }
    }
}

/// Represents a handle to a running redo-log instance.
#[derive(Debug)]
pub struct StdinLogReader {
    pid: Pid,
    stderr_fd: RawFd,
}

impl Drop for StdinLogReader {
    /// Await the redo-log instance we redirected stderr to.
    fn drop(&mut self) {
        // never actually close fd#1 or fd#2; insanity awaits.
        // replace it with something else instead.
        // Since our stdout/stderr are attached to redo-log's stdin,
        // this will notify redo-log that it's time to die (after it finishes
        // reading the logs)
        unistd::dup2(self.stderr_fd, 1).expect("could not restore stdout");
        unistd::dup2(self.stderr_fd, 2).expect("could not restore stderr");
        wait::waitpid(Some(self.pid), None).expect("failed to wait on log reader");
    }
}

fn nice<P: AsRef<RedoPath>>(env: &Env, t: P) -> io::Result<RedoPathBuf> {
    use std::convert::TryInto;
    state::relpath(t.as_ref(), env.startdir())?
        .try_into()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
}
