use nix;
use nix::fcntl::{self, FcntlArg, FdFlag};
use std::os::unix::io::AsRawFd;

pub(crate) fn close_on_exec<F: AsRawFd>(f: &F, yes: bool) -> nix::Result<()> {
  let fd = f.as_raw_fd();
  let result = fcntl::fcntl(fd, FcntlArg::F_GETFD)?;
  let mut fl = unsafe { FdFlag::from_bits_unchecked(result) };
  fl.set(FdFlag::FD_CLOEXEC, yes);
  fcntl::fcntl(fd, fcntl::F_SETFD(fl))?;
  Ok(())
}
