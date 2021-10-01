//! Implementation of a GNU make-compatible jobserver."""
//!
//! The basic idea is that both ends of a pipe (tokenfds) are shared with all
//! subprocesses.  At startup, we write one "token" into the pipe for each
//! configured job. (So eg. redo -j20 will put 20 tokens in the pipe.)  In
//! order to do work, you must first obtain a token, by reading the other
//! end of the pipe.  When you're done working, you write the token back into
//! the pipe so that someone else can grab it.
//!
//! The toplevel process in the hierarchy is what creates the pipes in the
//! first place.  Then it puts the pipe file descriptor numbers into MAKEFLAGS,
//! so that subprocesses can pull them back out.
//!
//! As usual, edge cases make all this a bit tricky:
//!
//! - Every process is defined as owning a token at startup time.  This makes
//!   sense because it's backward compatible with single-process make: if a
//!   subprocess neither reads nor writes the pipe, then it has exactly one
//!   token, so it's allowed to do one thread of work.
//!
//! - Thus, for symmetry, processes also must own a token at exit time.
//!
//! - In turn, to make *that* work, a parent process must destroy *its* token
//!   upon launching a subprocess.  (Destroy, not release, because the
//!   subprocess has created its own token.) It can try to obtain another
//!   token, but if none are available, it has to stop work until one of its
//!   subprocesses finishes.  When the subprocess finishes, its token is
//!   destroyed, so the parent creates a new one.
//!
//! - If our process is going to stop and wait for a lock (eg. because we
//!   depend on a target and someone else is already building that target),
//!   we must give up our token.  Otherwise, we're sucking up a "thread" (a
//!   unit of parallelism) just to do nothing.  If enough processes are waiting
//!   on a particular lock, then the process building that target might end up
//!   with only a single token, and everything gets serialized.
//!
//! - Unfortunately this leads to a problem: if we give up our token, we then
//!   have to re-acquire a token before exiting, even if we want to exit with
//!   an error code.
//!
//! - redo-log wants to linearize output so that it always prints log messages
//!   in the order jobs were started; but because of the above, a job being
//!   logged might end up with no tokens for a long time, waiting for some
//!   other branch of the build to complete.
//!
//! As a result, we extend beyond GNU make's model and make things even more
//! complicated.  We add a second pipe, cheatfds, which we use to "cheat" on
//! tokens if our particular job is in the foreground (ie.  is the one
//! currently being tailed by redo-log -f).  We add at most one token per
//! redo-log instance.  If we are the foreground task, and we need a token,
//! and we don't have a token, and we don't have any subtasks (because if we
//! had a subtask, then we're not in the foreground), we synthesize our own
//! token by incrementing _mytokens and _cheats, but we don't read from
//! tokenfds.  Then, when it's time to give up our token again, we also won't
//! write back to tokenfds, so the synthesized token disappears.
//!
//! Of course, all that then leads to *another* problem: every process must
//! hold a *real* token when it exits, because its parent has given up a
//! *real* token in order to start this subprocess.  If we're holding a cheat
//! token when it's time to exit, then we can't meet this requirement.  The
//! obvious thing to do would be to give up the cheat token and wait for a
//! real token, but that might take a very long time, and if we're the last
//! thing preventing our parent from exiting, then redo-log will sit around
//! following our parent until we finally get a token so we can exit,
//! defeating the whole purpose of cheating.  Instead of waiting, we write our
//! "cheater" token to cheatfds.  Then, any task, upon noticing one of its
//! subprocesses has finished, will check to see if there are any tokens on
//! cheatfds; if so, it will remove one of them and *not* re-create its
//! child's token, thus destroying the cheater token from earlier, and restoring
//! balance.
//!
//! Sorry this is so complicated.  I couldn't think of a way to make it
//! simpler :)

use failure::{format_err, Error};
use libc::{self, c_int, timeval};
use nix::errno::Errno;
use nix::fcntl;
use nix::sys::select::{self, FdSet};
use nix::sys::signal::{self, SigHandler, Signal};
use nix::sys::time::TimeVal;
use nix::sys::wait::{self, WaitStatus};
use nix::unistd::{self, ForkResult, Pid};
use std::cmp;
use std::collections::HashMap;
use std::env::{self, VarError};
use std::fmt::{self, Debug};
use std::iter;
use std::os::unix::io::RawFd;
use std::process;
use std::time::Duration;

use super::helpers::{self, IntervalTimer, IntervalTimerValue};

struct Job {
    name: String,
    pid: Pid,
    done_func: Box<dyn FnOnce(String, i32)>,
}

impl<'a> Debug for Job {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Job")
            .field("name", &self.name)
            .field("pid", &self.pid)
            .field("donefunc", &())
            .finish()
    }
}

#[derive(Debug)]
pub(crate) struct JobServer {
    my_tokens: i32,
    cheats: i32,
    token_fds: (RawFd, RawFd),
    cheat_fds: (RawFd, RawFd),
    wait_fds: HashMap<RawFd, Job>,
}

impl JobServer {
    pub(crate) fn setup(max_jobs: i32) -> Result<JobServer, Error> {
        const CHEATFDS_VAR: &str = "REDO_CHEATFDS";
        const MAKEFLAGS_VAR: &str = "MAKEFLAGS";

        assert!(max_jobs >= 0);
        // TODO(soon): Salute MAKEFLAGS.
        let cheats = if max_jobs == 0 {
            match env::var(CHEATFDS_VAR) {
                Ok(v) => v,
                Err(VarError::NotPresent) => String::new(),
                Err(e) => return Err(e.into()),
            }
        } else {
            String::new()
        };
        let cheat_fds = {
            let from_env = if !cheats.is_empty() {
                let parsed: (Option<RawFd>, Option<RawFd>) = {
                    let mut parts = cheats.splitn(2, ',').fuse();
                    let maybe_a = parts.next().and_then(|s| s.parse().ok());
                    let maybe_b = parts.next().and_then(|s| s.parse().ok());
                    (maybe_a, maybe_b)
                };
                match parsed {
                    (Some(a), Some(b)) => {
                        if helpers::fd_exists(a) && helpers::fd_exists(b) {
                            Some((a, b))
                        } else {
                            // This can happen if we're called by a parent process who closes
                            // all "unknown" file descriptors (which is anti-social behaviour,
                            // but oh well, we'll warn about it if they close the jobserver
                            // fds in MAKEFLAGS, so just ignore it if it also happens here).
                            None
                        }
                    }
                    _ => return Err(format_err!("invalid REDO_CHEATFDS: {:?}", cheats)),
                }
            } else {
                None
            };
            match from_env {
                Some(cheat_fds) => cheat_fds,
                None => {
                    let (a, b) = make_pipe(102)?;
                    env::set_var(CHEATFDS_VAR, format!("{},{}", a, b));
                    (a, b)
                }
            }
        };
        // TODO(soon): Don't start server if MAKEFLAGS is set.
        let token_fds = make_pipe(100)?;
        let realmax = if max_jobs == 0 { 1 } else { max_jobs };
        let mut server = JobServer {
            my_tokens: 1,
            cheats: 0,
            token_fds,
            cheat_fds,
            wait_fds: HashMap::new(),
        };
        server.create_tokens(realmax - 1);
        server.release_except_mine()?;
        env::set_var(
            MAKEFLAGS_VAR,
            format!(
                " -j --jobserver-auth={0},{1} --jobserver-fds={0},{1}",
                token_fds.0, token_fds.1
            ),
        );
        Ok(server)
    }

    /// Wait for a subproc to die or, if `want_token`, `token_fds.0` to be readable.
    ///
    /// Does not actually read `token_fds.0` once it's readable (so someone else
    /// might read it before us).  This function returns after `max_delay` or
    /// when at least one subproc dies or (if `want_token`) for `token_fds.0` to
    /// be readable.
    fn wait(&mut self, want_token: bool, max_delay: Option<Duration>) -> Result<(), Error> {
        let mut rfds: FdSet = FdSet::new();
        for k in self.wait_fds.keys().copied() {
            rfds.insert(k);
        }
        if want_token {
            rfds.insert(self.token_fds.0);
        }
        assert!(rfds.highest().is_some());
        let mut max_delay: Option<TimeVal> =
            max_delay.map(|d| helpers::timeval_from_duration(&d).into());
        select::select(None, Some(&mut rfds), None, None, max_delay.as_mut())?;
        for fd in rfds.fds(None) {
            if fd == self.token_fds.0 {
                continue;
            }
            // redo subprocesses are expected to die without releasing their
            // tokens, so things are less likely to get confused if they
            // die abnormally.  Since a child has died, that means a token has
            // 'disappeared' and we now need to recreate it.
            let mut b: [u8; 1] = [0];
            match try_read(self.cheat_fds.0, &mut b) {
                Ok(Some(1)) => {
                    // someone exited with _cheats > 0, so we need to compensate
                    // by *not* re-creating a token now.
                }
                Ok(None) | Ok(Some(0)) => {
                    self.create_tokens(1);
                    if self.has_token() {
                        self.release_except_mine();
                    }
                }
                Err(e) => return Err(e.into()),
                Ok(Some(_)) => unreachable!("only 1 byte possible to read"),
            }
            unistd::close(fd)?;
            let pd = self.wait_fds.remove(&fd).unwrap();
            let rv = wait::waitpid(Some(pd.pid), None)?;
            assert_eq!(rv.pid(), Some(pd.pid));
            let status = match rv {
                WaitStatus::Exited(_, status) => status,
                WaitStatus::Signaled(_, signal, _) => -(signal as i32),
                _ => return Err(format_err!("unhandled process status: {:?}", rv)),
            };
            (pd.done_func)(pd.name, status);
        }
        Ok(())
    }

    /// Materialize and own `n` tokens.
    ///
    /// If there are any cheater tokens active, they each destroy one matching
    /// newly-created token.
    fn create_tokens(&mut self, n: i32) {
        assert!(n >= 0);
        assert!(self.cheats >= 0);
        for _ in 0..n {
            if self.cheats > 0 {
                self.cheats -= 1;
            } else {
                self.my_tokens += 1;
            }
        }
    }

    /// Destroy n tokens that are currently in our posession.
    fn destroy_tokens(&mut self, n: i32) {
        assert!(self.my_tokens >= n);
        self.my_tokens -= n;
    }

    fn release(&mut self, n: i32) -> nix::Result<()> {
        assert!(n >= 0);
        assert!(self.my_tokens >= n);
        let mut n_to_share = 0usize;
        for _ in 0..n {
            self.my_tokens -= 1;
            if self.cheats > 0 {
                self.cheats -= 1;
            } else {
                n_to_share += 1;
            }
        }
        assert!(self.my_tokens >= 0);
        assert!(self.cheats >= 0);
        if n_to_share > 0 {
            write_tokens(self.token_fds.1, n_to_share)?;
        }
        Ok(())
    }

    fn release_except_mine(&mut self) -> nix::Result<()> {
        assert!(self.my_tokens > 0);
        self.release(self.my_tokens - 1)
    }

    #[inline]
    pub(crate) fn has_token(&self) -> bool {
        assert!(self.my_tokens >= 0);
        self.my_tokens >= 1
    }

    /// Don't return until this process has a job token.
    ///
    /// - `reason`: the reason (for debugging purposes) we need a token.  Usually
    ///   the name of a target we want to build.
    /// - `max_delay`: the max time to wait for a token, or `None` if forever.
    ///
    /// `has_token` will report `true` after this function returns *unless*
    /// `max_delay` is not `None` and we timed out.
    fn ensure_token(&mut self, reason: &str, max_delay: Option<Duration>) -> Result<(), Error> {
        assert!(self.my_tokens <= 1);
        loop {
            if self.my_tokens >= 1 {
                debug_assert_eq!(self.my_tokens, 1);
                break;
            }
            debug_assert!(self.my_tokens < 1);
            self.wait(true, max_delay)?;
            if self.my_tokens >= 1 {
                break;
            }
            debug_assert!(self.my_tokens < 1);
            let mut b: [u8; 1] = [0];
            match try_read(self.token_fds.0, &mut b)? {
                Some(0) => {
                    return Err(format_err!("unexpected EOF on token read"));
                }
                Some(1) => {
                    self.my_tokens += 1;
                    break;
                }
                Some(_) => unreachable!("only reading 1 byte"),
                None => {
                    if max_delay.is_some() {
                        break;
                    }
                }
            }
        }
        debug_assert!(self.my_tokens <= 1);
        Ok(())
    }

    /// Wait for a job token to become available, or cheat if possible.
    ///
    /// If we already have a token, we return immediately.  If we have any
    /// processes running *and* we don't own any tokens, we wait for a process
    /// to finish, and then use that token.
    ///
    /// Otherwise, we're allowed to cheat.  We call cheatfunc() occasionally
    /// to consider cheating; if it returns n > 0, we materialize that many
    /// cheater tokens and return.
    ///
    /// `cheatfunc`: a function which returns n > 0 (usually 1) if we should
    /// cheat right now, because we're the "foreground" process that must
    /// be allowed to continue.
    pub(crate) fn ensure_token_or_cheat<C>(
        &mut self,
        reason: &str,
        mut cheat_func: C,
    ) -> Result<(), Error>
    where
        C: FnMut() -> Result<i32, Error>,
    {
        let mut backoff = Duration::from_millis(10);
        while !self.has_token() {
            while self.is_running() && !self.has_token() {
                // If we already have a subproc running, then effectively we
                // already have a token.  Don't create a cheater token unless
                // we're completely idle.
                self.ensure_token(reason, None);
            }
            self.ensure_token(reason, Some(cmp::min(Duration::from_secs(1), backoff)));
            backoff *= 2;
            if !self.has_token() {
                debug_assert_eq!(self.my_tokens, 0);
                let n = cheat_func()?;
                if n > 0 {
                    self.my_tokens += n;
                    self.cheats += n;
                    break;
                }
            }
        }
        Ok(())
    }

    #[inline]
    pub(crate) fn is_running(&self) -> bool {
        !self.wait_fds.is_empty()
    }

    /// Release or destroy all the tokens we own, in preparation for exit.
    pub(crate) fn force_return_tokens(&mut self) -> Result<(), Error> {
        let n = self.wait_fds.len();
        self.wait_fds.clear();
        self.create_tokens(n as i32);
        if self.has_token() {
            self.release_except_mine();
            assert_eq!(self.my_tokens, 1);
        }
        assert!(
            self.cheats <= self.my_tokens,
            "mytokens={}, cheats={}",
            self.my_tokens,
            self.cheats
        );
        assert!(
            self.cheats == 0 || self.cheats == 1,
            "cheats={}",
            self.cheats
        );
        if self.cheats > 0 {
            self.destroy_tokens(self.cheats);
            write_tokens(self.cheat_fds.1, self.cheats as usize)?;
        }
        Ok(())
    }

    /// Start a new job.
    ///
    /// # Panics
    ///
    /// If `has_token()` returns `false`.
    pub(crate) fn start<R, F, D>(
        &mut self,
        reason: R,
        job_func: F,
        done_func: D,
    ) -> Result<(), Error>
    where
        R: Into<String>,
        F: FnOnce() -> i32,
        D: FnOnce(String, i32) + 'static,
    {
        assert_eq!(self.my_tokens, 1);
        // Subprocesses always start with 1 token, so we have to destroy ours
        // in order for the universe to stay in balance.
        self.destroy_tokens(1);
        let (r, w) = make_pipe(50)?;
        match unsafe { unistd::fork()? } {
            ForkResult::Child => {
                if let Err(e) = unistd::close(r) {
                    process::exit(201);
                }
                process::exit(job_func());
            }
            ForkResult::Parent { child: pid } => {
                helpers::close_on_exec(r, true);
                unistd::close(w)?;
                self.wait_fds.insert(
                    r,
                    Job {
                        name: reason.into(),
                        pid,
                        done_func: Box::new(done_func),
                    },
                );
                Ok(())
            }
        }
    }
}

impl Drop for JobServer {
    fn drop(&mut self) {
        let _ = self.force_return_tokens();
    }
}

/// We make the pipes use the first available fd numbers starting at `startfd`.
/// This makes it easier to differentiate different kinds of pipes when using
/// strace.
fn make_pipe(startfd: RawFd) -> nix::Result<(RawFd, RawFd)> {
    let (a, b) = unistd::pipe()?;
    let fds = (
        fcntl::fcntl(a, fcntl::F_DUPFD(startfd))?,
        fcntl::fcntl(b, fcntl::F_DUPFD(startfd + 1))?,
    );
    unistd::close(a)?;
    unistd::close(b)?;
    Ok(fds)
}

/// Try to fill `buf` with bytes read from `fd`. Returns `Ok(Some(0))`
/// on EOF, `Ok(None)` if `EAGAIN`.
fn try_read(fd: RawFd, buf: &mut [u8]) -> nix::Result<Option<usize>> {
    // using djb's suggested way of doing non-blocking reads from a blocking
    // socket: http://cr.yp.to/unix/nonblock.html
    // We can't just make the socket non-blocking, because we want to be
    // compatible with GNU Make, and they can't handle it.
    let mut rfds = FdSet::new();
    rfds.insert(fd);
    let mut timeout = timeval {
        tv_sec: 0,
        tv_usec: 0,
    }
    .into();
    select::select(None, Some(&mut rfds), None, None, Some(&mut timeout))?;
    if !rfds.contains(fd) {
        return Ok(None);
    }
    // The socket is readable - but some other process might get there first.
    // We have to set an alarm() in case our read() gets stuck.
    let oldh = unsafe { signal::signal(Signal::SIGALRM, SigHandler::Handler(timeout_handler)) }?;
    const INTERVAL_VALUE: IntervalTimerValue = IntervalTimerValue {
        interval: Duration::from_millis(10),
        value: Duration::from_millis(10),
    };
    helpers::set_interval_timer(IntervalTimer::Real, &INTERVAL_VALUE)?; // emergency fallback
    let result = match unistd::read(fd, buf) {
        Ok(n) => Ok(Some(n)),
        Err(nix::Error::Sys(Errno::EINTR)) | Err(nix::Error::Sys(Errno::EAGAIN)) => Ok(None),
        Err(e) => Err(e),
    };
    helpers::set_interval_timer(IntervalTimer::Real, &IntervalTimerValue::default())?;
    unsafe { signal::signal(Signal::SIGALRM, oldh) }?;
    result
}

extern "C" fn timeout_handler(_: c_int) {}

fn write_tokens(fd: RawFd, n: usize) -> nix::Result<()> {
    let buf: Vec<u8> = iter::repeat(b't').take(n).collect();
    // TODO(someday): Retry if interrupted or short write.
    unistd::write(fd, &buf)?;
    Ok(())
}
