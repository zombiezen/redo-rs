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
use futures::future::FusedFuture;
use futures::stream::Stream;
use futures::task;
use futures::{pin_mut, select};
use libc::{self, c_int, timeval};
use nix::errno::Errno;
use nix::fcntl;
use nix::sys::select::{self, FdSet};
use nix::sys::signal::{self, SigHandler, Signal};
use nix::sys::time::TimeVal;
use nix::sys::wait::{self, WaitStatus};
use nix::unistd::{self, ForkResult, Pid};
use std::cell::RefCell;
use std::cmp;
use std::collections::{HashMap, VecDeque};
use std::env::{self, VarError};
use std::ffi::{OsStr, OsString};
use std::future::Future;
use std::iter;
use std::os::unix::io::RawFd;
use std::pin::Pin;
use std::process;
use std::rc::Rc;
use std::str;
use std::task::{Context, Poll, Waker};
use std::thread;
use std::time::{Duration, Instant};

use super::helpers::{self, IntervalTimer, IntervalTimerValue};

/// Metadata about a running job.
#[derive(Debug)]
pub(crate) struct Job {
    pid: Pid,
    state: Rc<RefCell<JobState>>,
}

impl Future for Job {
    type Output = i32;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<i32> {
        let mut state = self.state.borrow_mut();
        match state.exit_code {
            Some(exit_code) => Poll::Ready(exit_code),
            None => {
                state.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        }
    }
}

#[derive(Clone, Debug, Default)]
struct JobState {
    exit_code: Option<i32>,
    waker: Option<Waker>,
}

#[derive(Debug)]
pub struct JobServer {
    params: Rc<ServerParams>,
    state: Rc<RefCell<ServerState>>,
}

impl JobServer {
    pub fn setup(max_jobs: i32) -> Result<JobServer, Error> {
        const CHEATFDS_VAR: &str = "REDO_CHEATFDS";
        const MAKEFLAGS_VAR: &str = "MAKEFLAGS";

        assert!(max_jobs >= 0);
        let token_fds = match parse_makeflags(env::var_os("MAKEFLAGS").unwrap_or_default())? {
            Some((a, b)) => {
                if !helpers::fd_exists(a) || !helpers::fd_exists(b) {
                    log_err!("broken --jobserver-auth from parent process:\n");
                    log_err!("  using GNU make? prefix your Makefile rule with \"+\"\n");
                    log_err!(
                        "  otherwise, see https://redo.rtfd.io/en/latest/FAQParallel/#MAKEFLAGS\n"
                    );
                    // TODO(soon): ImmediateReturn(200)
                    return Err(format_err!("broken --jobserver-auth from parent process"));
                }
                match max_jobs {
                    0 => {
                        // user requested zero tokens, which means use the parent jobserver
                        // if it exists.
                        Some((a, b))
                    }
                    1 => {
                        // user requested exactly one token, which means they want to
                        // serialize us, even if the parent redo is running in parallel.
                        // That's pretty harmless, so allow it without a warning.
                        None
                    }
                    _ => {
                        // user requested more than one token, even though we have a parent
                        // jobserver, which is fishy.  Warn about it, like make does.
                        log_warn!(
                            "warning: -j{} forced in sub-redo; starting new jobserver.\n",
                            max_jobs
                        );
                        None
                    }
                }
            }
            None => None,
        };
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
        match token_fds {
            Some(token_fds) => Ok(JobServer {
                params: Rc::new(ServerParams {
                    token_fds,
                    cheat_fds,
                    top_level: 0,
                }),
                state: Rc::new(RefCell::new(ServerState::default())),
            }),
            None => {
                let token_fds = make_pipe(100)?;
                let realmax = if max_jobs == 0 { 1 } else { max_jobs };
                let mut state = ServerState::default();
                state.create_tokens(realmax - 1);
                state.release_except_mine(token_fds)?;
                let state = Rc::new(RefCell::new(state));
                let server = JobServer {
                    params: Rc::new(ServerParams {
                        token_fds,
                        cheat_fds,
                        top_level: realmax,
                    }),
                    state,
                };
                env::set_var(
                    MAKEFLAGS_VAR,
                    format!(
                        " -j --jobserver-auth={0},{1} --jobserver-fds={0},{1}",
                        token_fds.0, token_fds.1
                    ),
                );
                Ok(server)
            }
        }
    }

    /// Get a clonable handle to the server.
    pub fn handle(&self) -> JobServerHandle {
        JobServerHandle {
            params: self.params.clone(),
            state: self.state.clone(),
        }
    }

    /// Run all tasks in the job server to completion.
    pub fn block_on<T, F, E>(&mut self, f: F) -> Result<T, Error>
    where
        E: Into<Error>,
        F: Future<Output = Result<T, E>>,
    {
        // TODO(someday): Prevent calling recursively.
        let mut rfds: FdSet = FdSet::new();
        let waker = task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        pin_mut!(f);
        loop {
            match Future::poll(f.as_mut(), &mut cx) {
                Poll::Ready(x) => {
                    return x.map_err(|e| e.into());
                }
                Poll::Pending => {
                    let now = Instant::now();
                    let mut state = self.state.borrow_mut();

                    // If there are any timers that have triggered during the run, don't
                    // bother waiting on I/O. Wake their futures immediately.
                    let mut timers_fired = false;
                    while state.timers.front().map_or(false, |&(fire, _)| fire < now) {
                        let (_, w) = state.timers.pop_front().unwrap();
                        w.wake();
                        timers_fired = true;
                    }
                    if timers_fired {
                        continue;
                    }

                    rfds.clear();
                    for k in state.wait_fds.keys().copied() {
                        rfds.insert(k);
                    }
                    if !state.token_wakers.is_empty() {
                        rfds.insert(self.params.token_fds.0);
                    }
                    let next_timer_duration = state.timers.front().map(|&(fire, _)| fire - now);
                    if rfds.highest().is_none() {
                        // No I/O to wait on.
                        if let Some(d) = next_timer_duration {
                            thread::sleep(d);
                            continue;
                        }
                        return Err(format_err!("JobServer deadlock"));
                    }
                    let mut max_delay: Option<TimeVal> =
                        next_timer_duration.map(|d| helpers::timeval_from_duration(d).into());
                    select::select(None, Some(&mut rfds), None, None, max_delay.as_mut())?;

                    for fd in rfds.fds(None) {
                        if fd == self.params.token_fds.0 {
                            if let Some(w) = state.token_wakers.pop_front() {
                                state.token_ready = true;
                                w.wake();
                            }
                            continue;
                        }
                        // redo subprocesses are expected to die without releasing their
                        // tokens, so things are less likely to get confused if they
                        // die abnormally.  Since a child has died, that means a token has
                        // 'disappeared' and we now need to recreate it.
                        let mut b: [u8; 1] = [0];
                        match try_read(self.params.cheat_fds.0, &mut b) {
                            Ok(Some(1)) => {
                                // someone exited with _cheats > 0, so we need to compensate
                                // by *not* re-creating a token now.
                            }
                            Ok(None) | Ok(Some(0)) => {
                                state.create_tokens(1);
                                if state.has_token() {
                                    state.release_except_mine(self.params.token_fds)?;
                                }
                            }
                            Err(e) => return Err(e.into()),
                            Ok(Some(_)) => unreachable!("only 1 byte possible to read"),
                        }
                        unistd::close(fd)?;
                        let pd = state.wait_fds.remove(&fd).unwrap();
                        let rv = wait::waitpid(Some(pd.pid), None)?;
                        assert_eq!(rv.pid(), Some(pd.pid));
                        let status = match rv {
                            WaitStatus::Exited(_, status) => status,
                            WaitStatus::Signaled(_, signal, _) => -(signal as i32),
                            _ => return Err(format_err!("unhandled process status: {:?}", rv)),
                        };
                        {
                            let mut state = pd.state.borrow_mut();
                            state.exit_code = Some(status);
                            if let Some(w) = state.waker.take() {
                                w.wake();
                            }
                        }
                    }
                }
            }
        }
    }

    /// Release or destroy all the tokens we own, in preparation for exit.
    pub fn force_return_tokens(&mut self) -> Result<(), Error> {
        let mut state = self.state.borrow_mut();
        let n = state.wait_fds.len();
        state.wait_fds.clear();
        state.create_tokens(n as i32);
        if state.has_token() {
            state.release_except_mine(self.params.token_fds)?;
            assert_eq!(state.my_tokens, 1);
        }
        assert!(
            state.cheats <= state.my_tokens,
            "mytokens={}, cheats={}",
            state.my_tokens,
            state.cheats
        );
        assert!(
            state.cheats == 0 || state.cheats == 1,
            "cheats={}",
            state.cheats
        );
        if state.cheats > 0 {
            let cheats = state.cheats;
            state.destroy_tokens(cheats);
            write_tokens(self.params.cheat_fds.1, state.cheats as usize)?;
        }
        Ok(())
    }
}

impl Drop for JobServer {
    fn drop(&mut self) {
        let _ = self.force_return_tokens();
    }
}

/// Immutable information about a `JobServer`.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ServerParams {
    token_fds: (RawFd, RawFd),
    cheat_fds: (RawFd, RawFd),
    top_level: i32,
}

/// Mutable information about a `JobServer`.
#[derive(Debug)]
struct ServerState {
    my_tokens: i32,
    cheats: i32,
    wait_fds: HashMap<RawFd, Job>,

    token_ready: bool,
    token_wakers: VecDeque<Waker>,

    timers: VecDeque<(Instant, Waker)>,
}

impl Default for ServerState {
    #[inline]
    fn default() -> ServerState {
        ServerState {
            my_tokens: 1,
            cheats: 0,
            wait_fds: HashMap::new(),
            token_ready: false,
            token_wakers: VecDeque::new(),
            timers: VecDeque::new(),
        }
    }
}

impl ServerState {
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

    #[inline]
    fn is_running(&self) -> bool {
        !self.wait_fds.is_empty()
    }

    #[inline]
    fn has_token(&self) -> bool {
        assert!(self.my_tokens >= 0);
        self.my_tokens >= 1
    }

    fn release(&mut self, token_fds: (RawFd, RawFd), n: i32) -> nix::Result<()> {
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
            write_tokens(token_fds.1, n_to_share)?;
        }
        Ok(())
    }

    fn release_except_mine(&mut self, token_fds: (RawFd, RawFd)) -> nix::Result<()> {
        assert!(self.my_tokens > 0);
        self.release(token_fds, self.my_tokens - 1)
    }

    fn release_mine(&mut self, token_fds: (RawFd, RawFd)) -> nix::Result<()> {
        assert!(self.my_tokens >= 1);
        self.release(token_fds, 1)
    }
}

/// A cloneable handle to `JobServer` that starts jobs.
#[derive(Clone, Debug)]
pub struct JobServerHandle {
    params: Rc<ServerParams>,
    state: Rc<RefCell<ServerState>>,
}

impl JobServerHandle {
    #[inline]
    pub(crate) fn has_token(&self) -> bool {
        self.state.borrow().has_token()
    }

    #[inline]
    pub(crate) fn is_running(&self) -> bool {
        self.state.borrow().is_running()
    }

    /// Start a new job.
    ///
    /// # Panics
    ///
    /// If `has_token()` returns `false`.
    pub(crate) fn start<F>(&self, job_func: F) -> Result<Job, Error>
    where
        F: FnOnce() -> i32,
    {
        {
            let mut state = self.state.borrow_mut();
            assert_eq!(state.my_tokens, 1);
            // Subprocesses always start with 1 token, so we have to destroy ours
            // in order for the universe to stay in balance.
            state.destroy_tokens(1);
        }
        let (r, w) = make_pipe(50)?;
        match unsafe { unistd::fork()? } {
            ForkResult::Child => {
                if let Err(e) = unistd::close(r) {
                    log_err!("close read end of pipe: {}\n", e);
                    process::exit(201);
                }
                process::exit(job_func());
            }
            ForkResult::Parent { child: pid } => {
                helpers::close_on_exec(r, true)?;
                unistd::close(w)?;
                let job_state = Rc::new(RefCell::new(JobState::default()));
                self.state.borrow_mut().wait_fds.insert(
                    r,
                    Job {
                        pid,
                        state: job_state.clone(),
                    },
                );
                Ok(Job {
                    pid,
                    state: job_state,
                })
            }
        }
    }

    /// Return a future that is ready once a duration has elapsed.
    #[inline]
    pub(crate) fn sleep(&self, d: Duration) -> Sleep {
        Sleep {
            end: Instant::now() + d,
            done: false,
            state: self.state.clone(),
        }
    }

    /// Don't return until this process has a job token.
    ///
    /// - `reason`: the reason (for debugging purposes) we need a token.  Usually
    ///   the name of a target we want to build.
    ///
    /// `has_token` will report `true` after this function returns with `Ok`.
    async fn ensure_token(&self, _reason: &str) -> Result<(), Error> {
        use futures::stream::StreamExt;

        let mut token_stream = TokenStream {
            state: self.state.clone(),
        };
        assert!(self.state.borrow().my_tokens <= 1);
        loop {
            {
                let state = self.state.borrow();
                if state.my_tokens >= 1 {
                    debug_assert_eq!(state.my_tokens, 1);
                    break;
                }
                debug_assert!(state.my_tokens < 1);
            }
            token_stream.next().await;
            {
                let state = self.state.borrow();
                if state.my_tokens >= 1 {
                    break;
                }
                debug_assert!(state.my_tokens < 1);
            }
            let mut b: [u8; 1] = [0];
            match try_read(self.params.token_fds.0, &mut b)? {
                Some(0) => {
                    return Err(format_err!("unexpected EOF on token read"));
                }
                Some(1) => {
                    let mut state = self.state.borrow_mut();
                    state.my_tokens += 1;
                    break;
                }
                Some(_) => unreachable!("only reading 1 byte"),
                None => {
                    // Token may have been stolen.
                }
            }
        }
        debug_assert!(self.state.borrow().my_tokens <= 1);
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
    pub(crate) async fn ensure_token_or_cheat<C>(
        &self,
        reason: &str,
        mut cheat_func: C,
    ) -> Result<(), Error>
    where
        C: FnMut() -> Result<i32, Error>,
    {
        use futures::future::FutureExt;

        let mut backoff = Duration::from_millis(10);
        while !self.has_token() {
            loop {
                {
                    let state = self.state.borrow();
                    if !state.is_running() || state.has_token() {
                        break;
                    }
                }
                // If we already have a subproc running, then effectively we
                // already have a token.  Don't create a cheater token unless
                // we're completely idle.
                self.ensure_token(reason).await?;
            }
            let token_future = self.ensure_token(reason).fuse();
            let mut token_future = Box::pin(token_future);
            let mut timeout = self.sleep(cmp::min(Duration::from_secs(1), backoff)).fuse();
            let got_token = select! {
                res = token_future => res.map(|_| true),
                _ = timeout => Ok(false),
            }?;
            if got_token {
                return Ok(());
            }
            backoff *= 2;
            {
                let has_token = {
                    let state = self.state.borrow();
                    let has_token = state.has_token();
                    if !has_token {
                        debug_assert_eq!(state.my_tokens, 0);
                    }
                    has_token
                };
                if !has_token {
                    let n = cheat_func()?;
                    if n > 0 {
                        let mut state = self.state.borrow_mut();
                        state.my_tokens += n;
                        state.cheats += n;
                        return Ok(());
                    }
                }
            }
        }
        Ok(())
    }

    /// Return a future that complets when all running jobs are finished.
    pub(crate) fn wait_all(&self) -> AllJobsDone {
        AllJobsDone {
            params: self.params.clone(),
            done: false,
            state: self.state.clone(),
        }
    }

    pub(crate) fn release_mine(&self) -> Result<(), Error> {
        let mut state = self.state.borrow_mut();
        state.release_mine(self.params.token_fds)?;
        Ok(())
    }
}

/// A simple timer created by `JobServerHandle.sleep`.
#[derive(Debug)]
pub(crate) struct Sleep {
    end: Instant,
    done: bool,
    state: Rc<RefCell<ServerState>>,
}

impl Future for Sleep {
    type Output = Instant;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Instant> {
        let now = Instant::now();
        if now >= self.end {
            self.done = true;
            Poll::Ready(now)
        } else {
            // TODO(someday): Replace any old wakers in the queue.
            // TODO(someday): Drop any old wakers in the queue when dropped.
            let mut state = self.state.borrow_mut();
            let i = state.timers.partition_point(|&(fire, _)| fire <= self.end);
            state.timers.insert(i, (self.end, cx.waker().clone()));
            Poll::Pending
        }
    }
}

impl FusedFuture for Sleep {
    fn is_terminated(&self) -> bool {
        self.done
    }
}

impl Clone for Sleep {
    fn clone(&self) -> Sleep {
        Sleep {
            end: self.end,
            done: false,
            state: self.state.clone(),
        }
    }
}

/// A future that waits for all jobs in a `JobServer` to finish.
#[derive(Debug)]
pub(crate) struct AllJobsDone {
    params: Rc<ServerParams>,
    done: bool,
    state: Rc<RefCell<ServerState>>,
}

impl Future for AllJobsDone {
    type Output = Result<(), Error>;

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), Error>> {
        if self.done {
            return Poll::Ready(Err(format_err!(
                "AllJobsDone::poll called on finished future"
            )));
        }
        while self.state.borrow().my_tokens >= 2 {
            let res = {
                let mut state = self.state.borrow_mut();
                state.release(self.params.token_fds, 1)
            };
            if let Err(e) = res {
                self.done = true;
                return Poll::Ready(Err(e.into()));
            }
        }
        if !self.state.borrow().is_running() {
            self.done = true;
            if self.params.top_level != 0 {
                // If we're the toplevel and we're sure no child processes remain,
                // then we know we're totally idle.  Self-test to ensure no tokens
                // mysteriously got created/destroyed.
                if self.state.borrow().my_tokens >= 1 {
                    let res = self.state.borrow_mut().release_mine(self.params.token_fds);
                    if let Err(e) = res {
                        return Poll::Ready(Err(e.into()));
                    }
                }
                let mut tokens_buf = [0u8; 8192];
                let tokens = match try_read(self.params.token_fds.0, &mut tokens_buf) {
                    Ok(None) => 0,
                    Ok(Some(n)) => n,
                    Err(e) => {
                        return Poll::Ready(Err(e.into()));
                    }
                };
                let mut cheats_buf = [0u8; 8192];
                let cheats = match try_read(self.params.cheat_fds.0, &mut cheats_buf) {
                    Ok(None) => 0,
                    Ok(Some(n)) => n,
                    Err(e) => {
                        return Poll::Ready(Err(e.into()));
                    }
                };
                if (tokens - cheats) as i32 != self.params.top_level {
                    return Poll::Ready(Err(format_err!(
                        "on exit: expected {} tokens; found {}-{}",
                        self.params.top_level,
                        tokens,
                        cheats
                    )));
                }
                // TODO(someday): Retry if interrupted or short write.
                if let Err(e) = unistd::write(self.params.token_fds.1, &tokens_buf[..tokens]) {
                    return Poll::Ready(Err(e.into()));
                }
            }
            // note: when we return, we may have *no* tokens, not even our own!
            // If caller wants to continue, they might have to obtain one first.
            return Poll::Ready(Ok(()));
        }
        // We should only release our last token if we have remaining
        // children.  A terminating redo process should try to terminate while
        // holding a token, and if we have no children left, we might be
        // about to terminate.
        if self.state.borrow().my_tokens >= 1 {
            self.done = true;
            let res = self.state.borrow_mut().release_mine(self.params.token_fds);
            if let Err(e) = res {
                return Poll::Ready(Err(e.into()));
            }
        }
        // TODO(someday): Store waker
        Poll::Pending
    }
}

impl FusedFuture for AllJobsDone {
    fn is_terminated(&self) -> bool {
        self.done
    }
}

impl Clone for AllJobsDone {
    fn clone(&self) -> AllJobsDone {
        AllJobsDone {
            params: self.params.clone(),
            done: false,
            state: self.state.clone(),
        }
    }
}

/// A stream for `tokens_fds.0` becoming readable.
///
/// Does not actually read `token_fds.0` once it's readable, so someone else
/// might read it before us.
#[derive(Debug)]
struct TokenStream {
    state: Rc<RefCell<ServerState>>,
}

impl Stream for TokenStream {
    type Item = ();

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<()>> {
        let mut state = self.state.borrow_mut();
        if state.token_ready {
            state.token_ready = false;
            Poll::Ready(Some(()))
        } else {
            // TODO(someday): Replace any old wakers in the queue. For now, using FILO.
            // TODO(someday): Drop any old wakers in the queue when dropped.
            state.token_wakers.push_front(cx.waker().clone());
            Poll::Pending
        }
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, None)
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

fn parse_makeflags<S: AsRef<OsStr>>(flags: S) -> Result<Option<(RawFd, RawFd)>, Error> {
    use failure::ResultExt;
    use std::os::unix::ffi::OsStrExt;

    let flags = flags.as_ref();
    let flags = {
        let mut new_flags = OsString::with_capacity(flags.len() + 2 * OsStr::new(" ").len());
        new_flags.push(" ");
        new_flags.push(flags);
        new_flags.push(" ");
        new_flags
    };
    let flags = flags.as_bytes();
    const FIND1: &[u8] = b" --jobserver-auth="; // renamed in GNU make 4.2
    const FIND2: &[u8] = b" --jobserver-fds="; // fallback syntax
    let find = flags
        .windows(FIND1.len())
        .position(|w| w == FIND1)
        .map(|i| (FIND1, i))
        .or_else(|| {
            flags
                .windows(FIND2.len())
                .position(|w| w == FIND2)
                .map(|i| (FIND2, i))
        });
    match find {
        Some((find, ofs)) => {
            let s = &flags[ofs + find.len()..];
            let arg = str::from_utf8(&s[..s.iter().copied().position(|b| b == b' ').unwrap()])
                .context("invalid MAKEFLAGS")?;
            let comma = match arg.find(',') {
                Some(i) => i,
                None => return Err(format_err!("invalid --jobserver-auth: {}", arg)),
            };
            let a = str::parse::<RawFd>(&arg[..comma])
                .with_context(|_| format!("invalid --jobserver-auth: {}", arg))?;
            let b = str::parse::<RawFd>(&arg[comma + 1..])
                .with_context(|_| format!("invalid --jobserver-auth: {}", arg))?;
            Ok(Some((a, b)))
        }
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_job() {
        let mut server = JobServer::setup(1).unwrap();
        let job = server.handle().start(|| 4).unwrap();
        let rv = server
            .block_on(async { Result::<_, Error>::Ok(job.await) })
            .unwrap();
        assert_eq!(rv, 4);
    }

    #[test]
    fn sleep() {
        let mut server = JobServer::setup(1).unwrap();
        let start = Instant::now();
        const SLEEP_DURATION: Duration = Duration::from_millis(100);
        let timer = server.handle().sleep(SLEEP_DURATION);
        let awake = server
            .block_on(async { Result::<_, Error>::Ok(timer.await) })
            .unwrap();
        assert!(
            awake - start >= SLEEP_DURATION,
            "start={:?}, awake={:?}",
            start,
            awake
        );
    }

    #[test]
    fn sleep_concurrently_with_job() {
        use futures::future;
        let mut server = JobServer::setup(1).unwrap();
        let start = Instant::now();
        const SLEEP_DURATION: Duration = Duration::from_millis(100);
        let job = server
            .handle()
            .start(|| {
                thread::sleep(SLEEP_DURATION + SLEEP_DURATION);
                4
            })
            .unwrap();
        let timer = server.handle().sleep(SLEEP_DURATION);
        let (rv, awake) = server
            .block_on(async { Result::<_, Error>::Ok(future::join(job, timer).await) })
            .unwrap();
        assert_eq!(rv, 4);
        assert!(
            awake - start >= SLEEP_DURATION,
            "start={:?}, awake={:?}",
            start,
            awake
        );
    }

    #[test]
    fn parse_makeflags_test() {
        assert_eq!(parse_makeflags("").unwrap(), None);
        assert_eq!(
            parse_makeflags("--jobserver-auth=1,2").unwrap(),
            Some((1, 2))
        );
        assert_eq!(
            parse_makeflags("--jobserver-fds=1,2").unwrap(),
            Some((1, 2))
        );
        assert_eq!(
            parse_makeflags("--jobserver-fds=1,2 --jobserver-auth=3,4").unwrap(),
            Some((3, 4))
        );
    }
}
