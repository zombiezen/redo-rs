use std::fs::File;
use std::io;
use nix::unistd;

use super::env::Env;

fn close_stdin() -> io::Result<()> {
  let f = File::open("/dev/null")?;
  unistd::dup2(oldfd: RawFd, io::stdin().as_raw_fd())?;
  Ok(())
}

fn await_log_reader(env: &Env, stderr_fd: RawFd) -> io::Result<()> {
  if !env.log {
    return Ok(());
  }
  // TODO(now): Check for log reader PID and wait on it.

  // never actually close fd#1 or fd#2; insanity awaits.
  // replace it with something else instead.
  // Since our stdout/stderr are attached to redo-log's stdin,
  // this will notify redo-log that it's time to die (after it finishes
  // reading the logs)
  unistd::dup2(stderr_fd, io::stdout().as_raw_fd());
  unistd::dup2(stderr_fd, io::stderr().as_raw_fd());
}

#[derive(Debug)]
struct BuildJob {
}

impl BuildJob {
  fn start(&self) -> io::Result<()> {

  }
}
