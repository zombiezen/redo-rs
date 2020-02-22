use nix::{self, NixPath};
use nix::errno::Errno;
use nix::fcntl::{self, FcntlArg, FdFlag};
use nix::unistd;
use std::borrow::Cow;
use std::os::unix::io::AsRawFd;
use std::path::Path;

pub(crate) fn close_on_exec<F: AsRawFd>(f: &F, yes: bool) -> nix::Result<()> {
  let fd = f.as_raw_fd();
  let result = fcntl::fcntl(fd, FcntlArg::F_GETFD)?;
  let mut fl = unsafe { FdFlag::from_bits_unchecked(result) };
  fl.set(FdFlag::FD_CLOEXEC, yes);
  fcntl::fcntl(fd, fcntl::F_SETFD(fl))?;
  Ok(())
}

/// Delete a file at path `f` if it currently exists.
///
/// Unlike `unlink()`, does not return an error if the file didn't already
/// exist.
pub(crate) fn unlink<P: ?Sized + NixPath>(f: &P) -> nix::Result<()> {
  match unistd::unlink(f) {
    Err(nix::Error::Sys(Errno::ENOENT)) => Ok(()),
    res => res,
  }
}

pub(crate) fn abs_path<'p, 'q, P: AsRef<Path>, Q: AsRef<Path>>(cwd: &'p P, path: &'q Q) -> Cow<'q, Path> {
  let path = path.as_ref();
  if path.is_absolute() {
    Cow::Borrowed(path)
  } else {
    let mut a = cwd.as_ref().to_path_buf();
    a.push(path);
    Cow::Owned(a)
  }
}
