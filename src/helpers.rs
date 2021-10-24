// Copyright 2021 Ross Light
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

use libc::{c_int, itimerval, timeval};
use nix::errno::{self, Errno};
use nix::fcntl::{self, FcntlArg, FdFlag};
use nix::unistd;
use nix::{self, NixPath};
use std::borrow::Cow;
use std::char;
use std::ffi::{OsStr, OsString};
use std::iter::FusedIterator;
use std::os::unix::io::RawFd;
use std::path::{self, Path};
use std::time::Duration;

pub(crate) fn close_on_exec(fd: RawFd, yes: bool) -> nix::Result<()> {
    let result = fcntl::fcntl(fd, FcntlArg::F_GETFD)?;
    let mut fl = unsafe { FdFlag::from_bits_unchecked(result) };
    fl.set(FdFlag::FD_CLOEXEC, yes);
    fcntl::fcntl(fd, fcntl::F_SETFD(fl))?;
    Ok(())
}

pub(crate) fn fd_exists(fd: RawFd) -> bool {
    fcntl::fcntl(fd, FcntlArg::F_GETFD).is_ok()
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

pub(crate) fn abs_path<'p, 'q, P, Q>(cwd: &'p P, path: &'q Q) -> Cow<'q, Path>
where
    P: AsRef<Path> + ?Sized,
    Q: AsRef<Path> + ?Sized,
{
    let path = path.as_ref();
    if path.is_absolute() {
        Cow::Borrowed(path)
    } else {
        let mut a = cwd.as_ref().to_path_buf();
        a.push(path);
        Cow::Owned(a)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[allow(dead_code)]
pub(crate) enum IntervalTimer {
    Real,
    Profiler,
    Virtual,
}

impl From<IntervalTimer> for c_int {
    fn from(which: IntervalTimer) -> c_int {
        match which {
            IntervalTimer::Real => libc::ITIMER_REAL,
            IntervalTimer::Profiler => libc::ITIMER_PROF,
            IntervalTimer::Virtual => libc::ITIMER_VIRTUAL,
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct IntervalTimerValue {
    pub(crate) interval: Duration,
    /// Time until next expiration
    pub(crate) value: Duration,
}

impl From<itimerval> for IntervalTimerValue {
    #[inline]
    fn from(val: itimerval) -> IntervalTimerValue {
        IntervalTimerValue::from(&val)
    }
}

impl From<&itimerval> for IntervalTimerValue {
    #[inline]
    fn from(val: &itimerval) -> IntervalTimerValue {
        IntervalTimerValue {
            interval: duration_from_timeval(&val.it_interval),
            value: duration_from_timeval(&val.it_value),
        }
    }
}

impl From<IntervalTimerValue> for itimerval {
    #[inline]
    fn from(val: IntervalTimerValue) -> itimerval {
        itimerval::from(&val)
    }
}

impl From<&IntervalTimerValue> for itimerval {
    #[inline]
    fn from(val: &IntervalTimerValue) -> itimerval {
        itimerval {
            it_interval: timeval_from_duration(val.interval),
            it_value: timeval_from_duration(val.value),
        }
    }
}

#[inline]
pub(crate) fn duration_from_timeval(tv: &timeval) -> Duration {
    Duration::from_secs(tv.tv_sec as u64) + Duration::from_micros(tv.tv_usec as u64)
}

#[inline]
pub(crate) const fn timeval_from_duration(d: Duration) -> timeval {
    timeval {
        tv_sec: d.as_secs() as i64,
        tv_usec: d.subsec_micros() as i64,
    }
}

pub(crate) fn set_interval_timer(
    which: IntervalTimer,
    value: &IntervalTimerValue,
) -> nix::Result<IntervalTimerValue> {
    let mut old_value = itimerval {
        it_interval: timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
        it_value: timeval {
            tv_sec: 0,
            tv_usec: 0,
        },
    };
    let ret = {
        let new_value = itimerval::from(value);
        unsafe { setitimer(which.into(), &new_value, &mut old_value) }
    };
    if ret == 0 {
        Ok((&old_value).into())
    } else {
        let e = Errno::from_i32(errno::errno());
        Err(e.into())
    }
}

#[cfg(target_os = "linux")]
extern "C" {
    fn setitimer(which: c_int, new_value: *const itimerval, old_value: *mut itimerval) -> c_int;
}

/// Return the shortest path name equivalent to path by purely lexical
/// processing. It applies the following rules iteratively until no further
/// processing can be done:
///
/// 1. Replace multiple separator elements with a single one.
/// 2. Eliminate each `.` path name element (the current directory).
/// 3. Eliminate each inner `..` path name element (the parent directory)
///    along with the non-`..` element that precedes it.
/// 4. Eliminate `..` elements that begin a rooted path:
///    that is, replace `/..` by `/` at the beginning of a path,
///    assuming the separator is `/`.
///
/// The returned path ends in a slash only if it represents a root directory,
/// such as `/` on Unix or `C:\` on Windows.
///
/// Finally, any occurrences of slash are replaced by Separator.
///
/// If the result of this process is an empty string, `normpath` returns the
/// string `.`.
///
/// See also Rob Pike, [Lexical File Names in Plan 9 or Getting Dot-Dot Right](https://9p.io/sys/doc/lexnames.html).
pub fn normpath<'a, P: AsRef<Path> + ?Sized>(p: &'a P) -> Cow<'a, Path> {
    let original = p.as_ref();
    let (vol_len, p) = strip_volume_name(original);
    if p.as_os_str().is_empty() {
        let mut buf = OsString::from(p.as_os_str());
        buf.push(".");
        return Cow::Owned(buf.into());
    }
    let rooted = path::is_separator(OsBytes::new(&p).next().unwrap() as char);

    // Invariants:
    //	reading from path; r is index of next byte to process.
    //	dotdot is index in buf where .. must stop, either because
    //		it is the leading slash or it is a leading ../../.. prefix.
    let mut out = LazyBuf {
        path: p.as_os_str(),
        buf: None,
        w: 0,
        vol_and_path: original.as_os_str(),
        vol_len,
    };
    let mut r = OsBytes::new(&p);
    let mut dotdot = 0usize;
    if rooted {
        out.append(path::MAIN_SEPARATOR as u8);
        r.next();
        dotdot = 1;
    }
    while let Some(r0) = r.get(0) {
        if path::is_separator(r0 as char) {
            // empty path element
            r.next();
        } else if r0 == b'.'
            && r.get(1)
                .map(|b| path::is_separator(b as char))
                .unwrap_or(true)
        {
            // . element
            r.next();
        } else if r0 == b'.'
            && r.get(1) == Some(b'.')
            && r.get(2)
                .map(|b| path::is_separator(b as char))
                .unwrap_or(true)
        {
            // .. element: remove to last separator
            r.next();
            r.next();
            if out.w > dotdot {
                // can backtrack
                out.w -= 1;
                while out.w > dotdot && !path::is_separator(out.index(out.w) as char) {
                    out.w -= 1;
                }
            } else if !rooted {
                // cannot backtrack, but not rooted, so append .. element.
                if out.w > 0 {
                    out.append(path::MAIN_SEPARATOR as u8);
                }
                out.append(b'.');
                out.append(b'.');
                dotdot = out.w;
            }
        } else {
            // real path element.
            // add slash if needed
            if (rooted && out.w != 1) || (!rooted && out.w != 0) {
                out.append(path::MAIN_SEPARATOR as u8);
            }
            // copy element
            while let Some(r0) = r.get(0) {
                if path::is_separator(r0 as char) {
                    break;
                }
                out.append(r0);
                r.next();
            }
        }
    }

    // Turn empty string into "."
    if out.w == 0 {
        out.append(b'.');
    }

    match out.into_osstring() {
        Cow::Borrowed(s) => Cow::Borrowed(s.as_ref()),
        Cow::Owned(s) => Cow::Owned(s.into()),
    }
}

/// UTF-8 byte iterator over an `OsStr`.
#[cfg(unix)]
#[derive(Clone, Debug)]
#[repr(transparent)]
pub(crate) struct OsBytes<'a> {
    s: &'a OsStr,
}

#[cfg(unix)]
impl<'a> OsBytes<'a> {
    pub(crate) fn new<S: AsRef<OsStr> + ?Sized>(s: &'a S) -> OsBytes<'a> {
        OsBytes { s: s.as_ref() }
    }

    fn new_osstring(b: Vec<u8>) -> OsString {
        use std::os::unix::ffi::OsStringExt;
        OsString::from_vec(b)
    }

    #[inline]
    fn get(&self, i: usize) -> Option<u8> {
        use std::os::unix::ffi::OsStrExt;
        self.s.as_bytes().get(i).copied()
    }

    #[inline]
    fn osstr_slice(&self, n: usize) -> Cow<'a, OsStr> {
        use std::os::unix::ffi::OsStrExt;
        Cow::Borrowed(OsStr::from_bytes(&self.s.as_bytes()[..n]))
    }
}

#[cfg(unix)]
impl<'a> Iterator for OsBytes<'a> {
    type Item = u8;

    fn next(&mut self) -> Option<u8> {
        use std::os::unix::ffi::OsStrExt;
        let bytes = self.s.as_bytes();
        match bytes.first().copied() {
            None => None,
            Some(b) => {
                self.s = OsStr::from_bytes(&bytes[1..]).as_ref();
                Some(b)
            }
        }
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        use std::os::unix::ffi::OsStrExt;
        let path_bytes = self.s.as_bytes();
        (path_bytes.len(), Some(path_bytes.len()))
    }

    #[inline]
    fn count(self) -> usize {
        use std::os::unix::ffi::OsStrExt;
        self.s.as_bytes().len()
    }
}

impl<'a> FusedIterator for OsBytes<'a> {}

#[cfg(unix)]
fn strip_volume_name(p: &Path) -> (usize, &Path) {
    (0, p)
}

#[derive(Clone, Debug)]
struct LazyBuf<'a> {
    path: &'a OsStr,
    buf: Option<Vec<u8>>,
    w: usize,
    vol_and_path: &'a OsStr,
    vol_len: usize,
}

impl<'a> LazyBuf<'a> {
    fn index(&self, i: usize) -> u8 {
        match self.buf {
            None => OsBytes::new(&self.path).get(i).unwrap(),
            Some(ref buf) => buf[i],
        }
    }

    fn append(&mut self, c: u8) {
        let mut buf = match self.buf.take() {
            None => {
                if OsBytes::new(&self.path).get(self.w) == Some(c) {
                    self.w += 1;
                    return;
                }
                let mut buf = Vec::with_capacity(OsBytes::new(&self.path).count());
                buf.extend(OsBytes::new(&self.path).take(self.w));
                buf
            }
            Some(buf) => buf,
        };
        if self.w < buf.len() {
            buf.truncate(self.w);
        }
        buf.push(c);
        self.w += 1;
        self.buf = Some(buf);
    }

    fn into_osstring(mut self) -> Cow<'a, OsStr> {
        match self.buf.take() {
            None => OsBytes::new(self.vol_and_path).osstr_slice(self.vol_len + self.w),
            Some(buf) => Cow::Owned(OsBytes::new_osstring(if self.vol_len == 0 {
                buf
            } else {
                let mut new_buf = Vec::with_capacity(self.vol_len + buf.len());
                new_buf.extend(OsBytes::new(&self.vol_and_path).take(self.vol_len));
                new_buf.extend(buf);
                new_buf
            })),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! normpath_tests {
        ($($name:ident: $value:expr,)*) => {
            $(
                #[test]
                fn $name() {
                let (input, want) = $value;
                assert_eq!(want, normpath(&input).as_os_str());
                }
            )*
        }
    }

    normpath_tests!(
        normpath_clean_name: ("abc", "abc"),
        normpath_clean_one_slash: ("abc/def", "abc/def"),
        normpath_clean_multiple_slashes: ("a/b/c", "a/b/c"),
        normpath_clean_dot: (".", "."),
        normpath_clean_parent: ("..", ".."),
        normpath_clean_multiple_parents: ("../..", "../.."),
        normpath_clean_multiple_parents_with_file: ("../../abc", "../../abc"),
        normpath_clean_root_file: ("/abc", "/abc"),
        normpath_clean_root: ("/", "/"),
        normpath_empty: ("", "."),
        normpath_trailing_slash_name: ("abc/", "abc"),
        normpath_trailing_slash_one_slash: ("abc/def/", "abc/def"),
        normpath_trailing_slash_multiple_slashes: ("a/b/c/", "a/b/c"),
        normpath_trailing_slash_dot: ("./", "."),
        normpath_trailing_slash_parent: ("../", ".."),
        normpath_trailing_slash_multiple_parents: ("../../", "../.."),
        normpath_trailing_slash_multiple_parents_with_file: ("../../abc/", "../../abc"),
        normpath_double_slash_inner: ("abc//def//ghi", "abc/def/ghi"),
        normpath_double_slash_leading: ("//abc", "/abc"),
        normpath_triple_slash_leading: ("///abc", "/abc"),
        normpath_double_slash_leading_and_trailing: ("//abc//", "/abc"),
        normpath_double_slash_trailing: ("abc//", "abc"),
        normpath_dot_inner: ("abc/./def", "abc/def"),
        normpath_dot_root: ("/./abc/def", "/abc/def"),
        normpath_dot_trailing: ("abc/.", "abc"),
        normpath_parent_inner: ("abc/def/ghi/../jkl", "abc/def/jkl"),
        normpath_parent_double_inner: ("abc/def/../ghi/../jkl", "abc/jkl"),
        normpath_parent_trailing: ("abc/def/..", "abc"),
        normpath_parent_trailing_to_dot: ("abc/def/../..", "."),
        normpath_parent_trailing_to_root: ("/abc/def/../..", "/"),
        normpath_parent_trailing_to_parent: ("abc/def/../../..", ".."),
        normpath_parent_trailing_to_root_parent: ("/abc/def/../../..", "/"),
        normpath_parent_to_sibling: ("abc/def/../../../ghi/jkl/../../../mno", "../../mno"),
        normpath_parent_root_file: ("/../abc", "/abc"),
        normpath_combo1: ("abc/./../def", "def"),
        normpath_combo2: ("abc//./../def", "def"),
        normpath_combo3: ("abc/../../././../def", "../../def"),
    );
}
