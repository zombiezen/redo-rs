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

use clap::{App, Arg, ArgMatches};
use failure::{format_err, Error};
use nix::unistd::{self, Pid};
use rusqlite::TransactionBehavior;
use std::cmp;
use std::collections::{HashSet, VecDeque};
use std::env;
use std::fs::File;
use std::io::{self, BufReader};
use std::mem;
use std::os::unix::io::RawFd;
use std::path::{Path, PathBuf};
use std::process;
use std::thread;
use std::time::{Duration, Instant};

use redo::logs::{self, LogBuilder, Meta};
use redo::{self, Env, Lock, LockType, ProcessState, ProcessTransaction};

use super::{auto_bool_arg, log_flags};

pub(crate) fn run() -> Result<(), Error> {
    use failure::ResultExt;
    use std::io::Write;
    use std::os::unix::io::FromRawFd;

    let mut ls = LogState::new();
    let matches = App::new("redo-log")
        .about("Print past build logs.")
        .arg(Arg::from_usage(
            "-r, --recursive 'show build logs for dependencies too'",
        ))
        .arg(Arg::from_usage(
            "-u, --unchanged 'show lines for dependencies not needing to be rebuilt'",
        ))
        .arg(Arg::from_usage(
            "-f, --follow 'keep watching for more lines to be appended (like tail -f)'",
        ))
        .args(&log_flags())
        .arg(Arg::from_usage("--ack-fd=[fd] 'print REDO-OK to this fd upon starting'").hidden(true))
        .arg(Arg::from_usage("<target>..."))
        .get_matches();
    let targets: Vec<&str> = matches
        .values_of("target")
        .map(|v| v.collect())
        .unwrap_or_default();
    if targets.is_empty() {
        eprintln!("redo-log give at least one target; maybe \"all\"?");
        process::exit(1);
    }

    let mut env = Env::init(&targets)?;
    if let Some(d) = auto_bool_arg(&matches, "debug-locks").into() {
        env.set_debug_locks(d);
    }
    if let Some(d) = auto_bool_arg(&matches, "debug-pids").into() {
        env.set_debug_pids(d);
    }
    let mut ps = ProcessState::init(env)?;
    let status =
        auto_bool_arg(&matches, "status").unwrap_or_else(|| unistd::isatty(2).unwrap_or(false));
    LogBuilder::new()
        .parent_logs(false)
        .pretty(auto_bool_arg(&matches, "pretty").unwrap_or(true))
        .color(auto_bool_arg(&matches, "color"))
        .setup(ps.env(), io::stdout());
    if let Some(ack_fd) = matches.value_of("ack-fd") {
        // Write back to owner, to let them know we started up okay and
        // will be able to see their error output, so it's okay to close
        // their old stderr.
        let ack_fd = str::parse::<RawFd>(ack_fd).context("invalid --ack-fd value")?;
        let mut ack_file = unsafe { File::from_raw_fd(ack_fd) };
        ack_file
            .write(b"REDO-OK\n")
            .context("failed write to --ack-fd")?;
    }
    let mut queue: VecDeque<&str> = VecDeque::from(targets);
    let topdir = env::current_dir()?;
    while let Some(t) = queue.pop_front() {
        if t != "-" {
            logs::meta(
                "do",
                rel(&topdir, ".", t)?
                    .as_os_str()
                    .to_str()
                    .ok_or(format_err!("cannot format target as string"))?,
                Some(Pid::from_raw(0)),
            );
        }
        ls.catlog(&mut ps, &matches, status, t)?;
    }
    Ok(())
}

struct LogState {
    already: HashSet<String>,
    depth: Vec<String>,
    total_lines: i64,
    status: String,
    start_time: Instant,
}

impl LogState {
    fn new() -> LogState {
        LogState {
            already: HashSet::new(),
            depth: Vec::new(),
            total_lines: 0,
            status: String::new(),
            start_time: Instant::now(),
        }
    }

    /// Copy the given log content to our current log output device.
    fn catlog(
        &mut self,
        ps: &mut ProcessState,
        matches: &ArgMatches,
        show_status: bool,
        t: &str,
    ) -> Result<i64, Error> {
        use std::convert::{TryFrom, TryInto};
        use std::io::{BufRead, Write};

        let topdir = env::current_dir()?;
        let mut lines_written: i64 = 0;
        let mut interrupted: i64 = 0;
        if !self.already.insert(t.to_string()) {
            return Ok(0);
        }
        if t != "-" {
            self.depth.push(t.to_string());
        }
        self.fix_depth(ps);
        let mydir = Path::new(t).parent().unwrap_or_else(|| Path::new(""));
        let stdin = io::stdin();
        let (mut f, mut info): (Option<Box<dyn BufRead>>, Option<(i64, Lock, PathBuf)>) =
            if t == "-" {
                (Some(Box::new(stdin.lock())), None)
            } else {
                let fid = {
                    let mut ptx = ProcessTransaction::new(ps, TransactionBehavior::Deferred)?;
                    match redo::File::from_name(&mut ptx, t, false) {
                        Ok(sf) => sf.id(),
                        Err(e) if e.kind().is_not_found() => {
                            eprintln!(
                                "redo-log: [{}] {:?}: not known to redo.",
                                env::current_dir()?.as_os_str().to_string_lossy(),
                                t
                            );
                            process::exit(24);
                        }
                        Err(e) => return Err(e.into()),
                    }
                };
                let logname = redo::logname(ps.env(), fid);
                let mut loglock = ps.new_lock(i32::try_from(fid)? + redo::LOG_LOCK_MAGIC);
                loglock.wait_lock(LockType::Shared)?;
                (None, Some((fid, loglock, logname)))
            };
        let mut delay = Duration::from_millis(10);
        let mut was_locked =
            is_locked(ps, info.as_ref().and_then(|&(fid, ..)| fid.try_into().ok()))?;
        let mut line_head = String::new();
        let mut width = tty_width();
        loop {
            if f.is_none() {
                let (_, _, logname) = info.as_ref().unwrap();
                match File::open(logname) {
                    Ok(log_file) => {
                        f = Some(Box::new(BufReader::new(log_file)));
                    }
                    Err(e) if e.kind() == io::ErrorKind::NotFound => {
                        // ignore files without logs
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            let mut line = if let Some(f) = f.as_mut() {
                // Note: normally includes trailing \n.
                // In 'follow' mode, might get a line with no trailing \n
                // (eg. when ./configure is halfway through a test), which we
                // deal with below.
                let mut line = String::new();
                f.read_line(&mut line)?;
                line
            } else {
                String::new()
            };
            if line.is_empty() && (!matches.is_present("follow") || !was_locked) {
                // file not locked, and no new lines: done
                break;
            }
            if line.is_empty() {
                was_locked =
                    is_locked(ps, info.as_ref().and_then(|&(fid, ..)| fid.try_into().ok()))?;
                if matches.is_present("follow") {
                    // Don't display status line for extremely short-lived runs
                    if show_status
                        && Instant::now().duration_since(self.start_time) > Duration::from_secs(1)
                    {
                        width = tty_width();
                        let mut head =
                            format!("redo {} ", format_thousands(self.total_lines as u64));
                        let mut tail = String::new();
                        for n in self.depth.iter().rev() {
                            let remain = width - head.len() - tail.len();
                            // always leave room for a final '... ' prefix
                            if remain < n.len() + 4 + 1 || remain <= 4 {
                                if n.len() < 6 || remain < 6 + 1 + 4 {
                                    tail = format!("... {}", tail);
                                } else {
                                    let start = n.len() - (remain - 3 - 1);
                                    tail = format!("...{} {}", &n[start..], &tail);
                                }
                                break;
                            } else if n != "-" {
                                tail = format!("{} {}", n, tail);
                            }
                        }
                        head.push_str(&tail);
                        self.status = head;
                        if self.status.len() > width {
                            eprintln!(
                                "\nOVERSIZE STATUS ({}):\n{:?}",
                                self.status.len(),
                                &self.status
                            );
                        }
                        assert!(self.status.len() <= width);
                        io::stdout().flush()?;
                        eprint!("\r{:<width$.width$}\r", &self.status, width = width);
                    }
                    thread::sleep(cmp::min(delay, Duration::from_secs(1)));
                    delay += Duration::from_millis(10);
                }
                continue;
            }
            self.total_lines += 1;
            delay = Duration::from_millis(10);
            if !line.ends_with('\n') {
                line_head.push_str(&line);
                continue;
            }
            if !line_head.is_empty() {
                line_head.push_str(&line);
                line = String::new();
                mem::swap(&mut line, &mut line_head);
            }
            if !self.status.is_empty() {
                io::stdout().flush()?;
                eprint!("\r{:<width$.width$}\r", "", width = width);
                // TODO(maybe): Reuse status buffer between iterations.
                self.status = String::new();
            }
            match Meta::parse(line.trim_end_matches('\n')) {
                Ok(g) => {
                    // FIXME: print prefix if @@REDO is not at start of line.
                    //   logs::PrettyLog does it, but only if we actually call .write().
                    let relname = rel(&topdir, mydir, g.text())?
                        .into_os_string()
                        .into_string()
                        .expect("cannot format target as string");
                    let fixname = redo::normpath(&mydir.join(g.text()))
                        .into_owned()
                        .into_os_string()
                        .into_string()
                        .expect("cannot format target as string");
                    match g.kind() {
                        "unchanged" => {
                            if matches.is_present("unchanged") {
                                if auto_bool_arg(&matches, "debug-locks").unwrap_or(false) {
                                    logs::meta(g.kind(), &relname, Some(g.pid()));
                                } else if !self.already.contains(&fixname) {
                                    logs::meta("do", &relname, Some(g.pid()));
                                }
                                if matches.is_present("recursive") {
                                    if let Some((_, loglock, _)) = info.as_mut() {
                                        loglock.unlock()?;
                                    }
                                    let new_t = mydir
                                        .join(g.text())
                                        .into_os_string()
                                        .into_string()
                                        .expect("cannot format target as string");
                                    let got = self.catlog(ps, matches, show_status, &new_t)?;
                                    interrupted += got;
                                    lines_written += got;
                                    if let Some((_, loglock, _)) = info.as_mut() {
                                        loglock.wait_lock(LockType::Shared)?;
                                    }
                                }
                                self.already.insert(fixname);
                            }
                        }
                        "do" | "waiting" | "locked" | "unlocked" => {
                            if auto_bool_arg(&matches, "debug-locks").unwrap_or(false) {
                                logs::meta(g.kind(), &relname, Some(g.pid()));
                                logs::write(line.trim_end());
                                interrupted += 1;
                                lines_written += 1;
                            } else if !self.already.contains(&fixname) {
                                logs::meta("do", &relname, Some(g.pid()));
                                interrupted += 1;
                                lines_written += 1;
                            }
                            if matches.is_present("recursive") {
                                assert!(!g.text().is_empty());
                                if let Some((_, loglock, _)) = info.as_mut() {
                                    loglock.unlock()?;
                                }
                                let new_t = mydir
                                    .join(g.text())
                                    .into_os_string()
                                    .into_string()
                                    .expect("cannot format target as string");
                                let got = self.catlog(ps, matches, show_status, &new_t)?;
                                interrupted += got;
                                lines_written += got;
                                if let Some((_, loglock, _)) = info.as_mut() {
                                    loglock.wait_lock(LockType::Shared)?;
                                }
                            }
                            self.already.insert(fixname);
                        }
                        "done" => {
                            let (rv, name) =
                                g.done_text().expect("improperly formatted done entry");
                            logs::meta(
                                g.kind(),
                                &format!(
                                    "{} {}",
                                    rv,
                                    rel(&topdir, mydir, name)?
                                        .into_os_string()
                                        .into_string()
                                        .expect("cannot format target as string")
                                ),
                                None,
                            );
                            lines_written += 1;
                        }
                        _ => {
                            logs::write(line.trim_end());
                            lines_written += 1;
                        }
                    }
                }
                Err(_) => {
                    if auto_bool_arg(&matches, "details").unwrap_or(true) {
                        if interrupted != 0 {
                            let d = ps.env().depth().len();
                            ps.env_mut().set_depth(d - 2);
                            logs::meta("resumed", t, None);
                            ps.env_mut().set_depth(d);
                            interrupted = 0;
                        }
                        logs::write(line.trim_end());
                        lines_written += 1;
                    }
                }
            }
        }
        mem::drop(info); // unlock
        if !self.status.is_empty() {
            io::stdout().flush()?;
            eprint!("\r{:<width$.width$}\r", "", width = width);
            self.status = String::new();
        }
        if !line_head.is_empty() {
            // partial line never got terminated
            print!("{}", line_head);
        }
        if t != "-" {
            let last = self.depth.pop();
            assert_eq!(last.as_ref().map(|s| -> &str { &*s }), Some(t));
        }
        self.fix_depth(ps);
        Ok(lines_written)
    }

    fn fix_depth(&self, ps: &mut ProcessState) {
        ps.env_mut().set_depth(self.depth.len() * 2);
    }
}

fn tty_width() -> usize {
    term_size::dimensions_stderr()
        .map(|(w, _)| w)
        .or_else(|| {
            env::var("WIDTH")
                .ok()
                .and_then(|s| str::parse::<usize>(&s).ok())
        })
        .unwrap_or(70)
}

/// Format an integer with thousands separators.
fn format_thousands(n: u64) -> String {
    // TODO(someday): Less allocations.
    let orig = n.to_string();
    String::from_utf8(
        orig.bytes()
            .rev()
            .enumerate()
            .flat_map(|(i, c)| {
                if i > 0 && i % 3 == 0 {
                    vec![b',', c]
                } else {
                    vec![c]
                }
            })
            .rev()
            .collect(),
    )
    .unwrap()
}

fn is_locked(ps: &ProcessState, fid: Option<i32>) -> Result<bool, Error> {
    let fid = match fid {
        Some(fid) => fid,
        None => return Ok(false),
    };
    // This acquires and immediately releases the lock (via Drop).
    let mut lock = ps.new_lock(fid);
    Ok(!lock.try_lock()?)
}

fn rel<P1, P2, P3>(top: P1, mydir: P2, path: P3) -> io::Result<PathBuf>
where
    P1: AsRef<Path>,
    P2: AsRef<Path>,
    P3: AsRef<Path>,
{
    let topdir = env::current_dir()?;
    let mut p = PathBuf::new();
    p.push(top);
    p.push(mydir);
    p.push(path);
    redo::relpath(p, topdir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_thousands_zero() {
        assert_eq!("0", &format_thousands(0));
    }

    #[test]
    fn format_thousands_one() {
        assert_eq!("1", &format_thousands(1));
    }

    #[test]
    fn format_thousands_one_thousand() {
        assert_eq!("1,000", &format_thousands(1000));
    }

    #[test]
    fn format_thousands_1234() {
        assert_eq!("1,234", &format_thousands(1234));
    }
}
