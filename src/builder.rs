use failure::{Backtrace, Context, Error, Fail, ResultExt};
use nix::unistd;
use rusqlite::{DropBehavior, TransactionBehavior};
use std::collections::HashSet;
use std::convert::TryInto;
use std::fmt::{self, Display};
use std::fs::File;
use std::io;
use std::os::unix::io::RawFd;
use std::path::PathBuf;

use super::env::Env;
use super::jobserver::JobServer;
use super::state::{self, Lock, ProcessState, ProcessTransaction};

pub(crate) fn close_stdin() -> Result<(), Error> {
    use std::os::unix::io::AsRawFd;
    let f = File::open("/dev/null")?;
    unistd::dup2(f.as_raw_fd(), io::stdin().as_raw_fd())?;
    Ok(())
}

pub(crate) fn await_log_reader(env: &Env, stderr_fd: RawFd) -> Result<(), Error> {
    use std::os::unix::io::AsRawFd;
    if env.log == 0 {
        return Ok(());
    }
    if false {
        // TODO(now): Check for log reader PID and wait on it.

        // never actually close fd#1 or fd#2; insanity awaits.
        // replace it with something else instead.
        // Since our stdout/stderr are attached to redo-log's stdin,
        // this will notify redo-log that it's time to die (after it finishes
        // reading the logs)
        unistd::dup2(stderr_fd, io::stdout().as_raw_fd())?;
        unistd::dup2(stderr_fd, io::stderr().as_raw_fd())?;
    }
    Ok(())
}

#[derive(Debug)]
struct BuildJob {}

impl BuildJob {
    fn start(&self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) fn run<S: AsRef<str>>(
    ps: &mut ProcessState,
    server: &mut JobServer,
    targets: &[S],
) -> Result<(), BuildError> {
    for t in targets {
        let t = t.as_ref();
        if t.find('\n').is_some() {
            return Err(BuildErrorKind::InvalidTarget(t.into()).into());
        }
    }

    let mut me: Option<(PathBuf, state::File, Lock)> =
        if !ps.env().target.as_os_str().is_empty() && !ps.env().unlocked {
            let mut me = PathBuf::from(&ps.env().startdir);
            me.push(&ps.env().pwd);
            me.push(&ps.env().target);
            let myfile = {
                let mut ptx = ProcessTransaction::new(ps, TransactionBehavior::Deferred)
                    .context(BuildErrorKind::Generic)?;
                ptx.set_drop_behavior(DropBehavior::Commit);
                state::File::from_name(&mut ptx, &me.to_string_lossy(), true)
                    .context(BuildErrorKind::Generic)?
            };
            let selflock = ps.new_lock(state::LOG_LOCK_MAGIC + (myfile.id as i32));
            Some((me, myfile, selflock))
        } else {
            None
        };

    let mut result: Result<(), BuildError> = Ok(());
    let mut locked: Vec<(i64, &str)> = Vec::new();
    let mut cheat = || -> Result<i32, Error> {
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
    {
        let mut seen: HashSet<String> = HashSet::new();
        for t in targets {
            let t = t.as_ref();
            if t.is_empty() {
                result = Err(BuildErrorKind::InvalidTarget(t.into()).into());
                break;
            }
            assert!(ps.is_flushed());
            if seen.contains(t) {
                continue;
            }
            seen.insert(t.into());
            // TODO(maybe): Commit state if !has_token.
            server
                .ensure_token_or_cheat(t, &mut cheat)
                .context(BuildErrorKind::Generic)?;
            if result.is_err() && !ps.env().keep_going {
                break;
            }
            // TODO(soon): state.check_sane.
            {
                let mut ptx = ProcessTransaction::new(ps, TransactionBehavior::Deferred)
                    .context(BuildErrorKind::Generic)?;
                ptx.set_drop_behavior(DropBehavior::Commit);
                let mut f =
                    state::File::from_name(&mut ptx, t, true).context(BuildErrorKind::Generic)?;
                let mut lock = ptx.state().new_lock(f.id.try_into().unwrap());
                if ptx.state().env().unlocked {
                    lock.force_owned();
                } else {
                    lock.try_lock().context(BuildErrorKind::Generic)?;
                }
                if !lock.is_owned() {
                    // TODO(soon): meta something?
                    locked.push((f.id, t));
                } else {
                    // We had to create f before we had a lock, because we need f.id
                    // to make the lock.  But someone may have updated the state
                    // between then and now.
                    // FIXME: separate obtaining the fid from creating the File.
                    // FIXME: maybe integrate locking into the File object?
                    f.refresh(&mut ptx).context(BuildErrorKind::Generic)?;
                    // TODO(now): Start build job.
                }
            }
            assert!(ps.is_flushed());
        }
    }
    result
}

#[derive(Debug)]
pub(crate) struct BuildError {
    inner: Context<BuildErrorKind>,
}

#[derive(Clone, Eq, PartialEq, Debug, Fail)]
#[non_exhaustive]
pub(crate) enum BuildErrorKind {
    #[fail(display = "Build failed")]
    Generic,
    #[fail(display = "Invalid target {:?}", _0)]
    InvalidTarget(String),
}

impl BuildError {
    #[inline]
    pub(crate) fn kind(&self) -> &BuildErrorKind {
        self.inner.get_context()
    }
}

impl Fail for BuildError {
    fn cause(&self) -> Option<&dyn Fail> {
        self.inner.cause()
    }

    fn backtrace(&self) -> Option<&Backtrace> {
        self.inner.backtrace()
    }
}

impl Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        Display::fmt(&self.inner, f)
    }
}

impl From<BuildErrorKind> for BuildError {
    fn from(kind: BuildErrorKind) -> BuildError {
        BuildError {
            inner: Context::new(kind),
        }
    }
}

impl From<Context<BuildErrorKind>> for BuildError {
    fn from(inner: Context<BuildErrorKind>) -> BuildError {
        BuildError { inner: inner }
    }
}

impl Default for BuildErrorKind {
    #[inline]
    fn default() -> BuildErrorKind {
        BuildErrorKind::Generic
    }
}

impl From<&BuildErrorKind> for i32 {
    fn from(kind: &BuildErrorKind) -> i32 {
        match kind {
            BuildErrorKind::InvalidTarget(_) => 204,
            _ => 1,
        }
    }
}
