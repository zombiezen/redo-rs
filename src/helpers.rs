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

use libc::{c_int, itimerval, suseconds_t, time_t, timeval};
use nix::errno::{self, Errno};
use nix::fcntl::{self, FcntlArg, FdFlag};
use nix::unistd;
use nix::{self, NixPath};
use rusqlite::types::{FromSql, FromSqlError, ToSql, ToSqlOutput};
use std::borrow::{Borrow, Cow};
use std::char;
use std::convert::TryFrom;
use std::ffi::{CStr, CString, OsStr, OsString};
use std::fmt::{self, Debug, Display, Formatter};
use std::iter::FusedIterator;
use std::mem;
use std::ops::Deref;
use std::os::unix::io::RawFd;
use std::path::{self, Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use super::error::{RedoError, RedoErrorKind};

/// A slice of a path (akin to [`str`]).
///
/// This type guarantees that the path contains no nul bytes or newline bytes
/// and is valid UTF-8.
#[derive(Eq, PartialEq, Hash, Ord, PartialOrd)]
#[repr(transparent)]
pub struct RedoPath(OsStr);

impl RedoPath {
    /// Coerces a UTF-8 string into a `RedoPath`.
    ///
    /// # Errors
    ///
    /// If the string contains any nul bytes, an error variant will be returned.
    pub fn from_str<S: AsRef<str> + ?Sized>(s: &S) -> Result<&RedoPath, RedoPathError> {
        let s = s.as_ref();
        if RedoPath::validate(s) {
            Ok(unsafe { RedoPath::from_str_unchecked(s) })
        } else {
            Err(RedoPathError { input: s.into() })
        }
    }

    fn validate(s: &str) -> bool {
        !s.contains(|c| c == '\0' || c == '\n')
    }

    /// Coerces a UTF-8 string into a `RedoPath` without any runtime checks.
    pub unsafe fn from_str_unchecked<S: AsRef<str> + ?Sized>(s: &S) -> &RedoPath {
        RedoPath::from_os_str_unchecked(OsStr::new(s.as_ref()))
    }

    /// Coerces a platform-native string into a `RedoPath`.
    ///
    /// # Errors
    ///
    /// If the string contains any nul bytes or is not valid UTF-8, an error
    /// variant will be returned.
    pub fn from_os_str<S: AsRef<OsStr> + ?Sized>(s: &S) -> Result<&RedoPath, RedoPathError> {
        let s = s.as_ref();
        match s.to_str() {
            Some(s) => RedoPath::from_str(s),
            None => Err(RedoPathError {
                input: s.to_os_string(),
            }),
        }
    }

    /// Coerces a platform-native string into a `RedoPath` without any runtime checks.
    #[inline]
    pub unsafe fn from_os_str_unchecked<S: AsRef<OsStr> + ?Sized>(s: &S) -> &RedoPath {
        mem::transmute(s.as_ref())
    }

    /// Reports whether the `RedoPath` is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Yields the underlying [`OsStr`] slice.
    #[inline]
    pub fn as_os_str(&self) -> &OsStr {
        &self.0
    }

    /// Yields the underlying [`OsStr`] slice as a [`Path`].
    #[inline]
    pub fn as_path(&self) -> &Path {
        Path::new(self.as_os_str())
    }

    /// Yields the underlying [`str`] slice.
    #[inline]
    pub fn as_str(&self) -> &str {
        self.0.to_str().unwrap()
    }

    /// Creates a C-compatible string.
    #[inline]
    pub fn to_c_string(&self) -> CString {
        let v = self.as_str().as_bytes().to_vec();
        unsafe { CString::from_vec_unchecked(v) }
    }

    /// Copies the slice into an owned `RedoPathBuf`.
    #[inline]
    pub fn to_redo_path_buf(&self) -> RedoPathBuf {
        let s = self.0.to_os_string();
        unsafe { RedoPathBuf::from_os_string_unchecked(s) }
    }

    /// Creates an owned [`RedoPathBuf`] with `path` adjoined to `self`.
    pub fn join<P: AsRef<RedoPath>>(&self, path: P) -> RedoPathBuf {
        let s = self.as_path().join(path.as_ref()).into_os_string();
        unsafe { RedoPathBuf::from_os_string_unchecked(s) }
    }

    /// Returns the `RedoPath` without its final component, if there is one.
    pub fn parent(&self) -> Option<&RedoPath> {
        self.as_path()
            .parent()
            .map(|p| unsafe { RedoPath::from_os_str_unchecked(p) })
    }

    /// Returns the final component of the `RedoPath`, if there is one.
    pub fn file_name(&self) -> Option<&RedoPath> {
        self.as_path()
            .file_name()
            .map(|s| unsafe { RedoPath::from_os_str_unchecked(s) })
    }

    /// Return the shortest path name equivalent to path by purely lexical
    /// processing.
    ///
    /// See [`normpath`] for details.
    pub fn normpath(&self) -> Cow<RedoPath> {
        match normpath(self) {
            Cow::Borrowed(p) => Cow::Borrowed(unsafe { RedoPath::from_os_str_unchecked(p) }),
            Cow::Owned(p) => {
                Cow::Owned(unsafe { RedoPathBuf::from_os_string_unchecked(p.into_os_string()) })
            }
        }
    }
}

impl Default for &RedoPath {
    #[inline]
    fn default() -> Self {
        unsafe { RedoPath::from_os_str_unchecked(OsStr::new("")) }
    }
}

impl AsRef<RedoPath> for RedoPath {
    #[inline]
    fn as_ref(&self) -> &RedoPath {
        self
    }
}

impl AsRef<str> for RedoPath {
    #[inline]
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl AsRef<OsStr> for RedoPath {
    #[inline]
    fn as_ref(&self) -> &OsStr {
        self.as_os_str()
    }
}

impl AsRef<Path> for RedoPath {
    #[inline]
    fn as_ref(&self) -> &Path {
        self.as_path()
    }
}

impl ToSql for RedoPath {
    fn to_sql(&self) -> Result<ToSqlOutput, rusqlite::Error> {
        self.as_str().to_sql()
    }
}

impl<'a> From<&'a RedoPath> for Cow<'a, RedoPath> {
    #[inline]
    fn from(p: &'a RedoPath) -> Cow<'a, RedoPath> {
        Cow::Borrowed(p)
    }
}

impl Debug for RedoPath {
    #[inline]
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        Debug::fmt(self.as_str(), f)
    }
}

impl Display for RedoPath {
    #[inline]
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl ToOwned for RedoPath {
    type Owned = RedoPathBuf;

    #[inline]
    fn to_owned(&self) -> RedoPathBuf {
        self.to_redo_path_buf()
    }
}

impl NixPath for RedoPath {
    fn is_empty(&self) -> bool {
        RedoPath::is_empty(self)
    }

    fn len(&self) -> usize {
        self.as_str().as_bytes().len()
    }

    fn with_nix_path<T, F>(&self, f: F) -> Result<T, nix::Error>
    where
        F: FnOnce(&CStr) -> T,
    {
        let cs = self.to_c_string();
        Ok(f(&cs))
    }
}

/// A type that represents owned, mutable platform-native strings.
///
/// This type guarantees that the path contains no nul bytes and is valid UTF-8.
#[derive(Clone, Eq, PartialEq, Hash, Ord, PartialOrd)]
#[repr(transparent)]
pub struct RedoPathBuf(OsString);

impl RedoPathBuf {
    /// Constructs a new empty `RedoPathBuf`.
    #[inline]
    pub fn new() -> RedoPathBuf {
        RedoPathBuf(OsString::new())
    }

    /// Coerces a UTF-8 string into a `RedoPathBuf` without any runtime checks.
    ///
    /// This conversion does not allocate or copy memory.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the string does not contain any nul bytes.
    #[inline]
    pub unsafe fn from_string_unchecked(s: String) -> RedoPathBuf {
        RedoPathBuf::from_os_string_unchecked(OsString::from(s))
    }

    /// Coerces a platform-native string into a `RedoPath` without any runtime checks.
    ///
    /// This conversion does not allocate or copy memory.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the string contains valid UTF-8 and does not
    /// contain any nul bytes.
    #[inline]
    pub unsafe fn from_os_string_unchecked(s: OsString) -> RedoPathBuf {
        RedoPathBuf(s)
    }

    /// Coerces to a [`RedoPath`] slice.
    #[inline]
    pub fn as_redo_path(&self) -> &RedoPath {
        let p = self.0.as_os_str();
        unsafe { RedoPath::from_os_str_unchecked(p) }
    }

    /// Converts the `RedoPathBuf` into an [`OsString`].
    #[inline]
    pub fn into_os_string(self) -> OsString {
        self.0
    }

    /// Converts the `RedoPathBuf` into a [`PathBuf`].
    #[inline]
    pub fn into_path_buf(self) -> PathBuf {
        PathBuf::from(self.0)
    }

    /// Converts the `RedoPathBuf` into a [`String`].
    #[inline]
    pub fn into_string(self) -> String {
        self.0.into_string().unwrap()
    }
}

impl Default for RedoPathBuf {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl TryFrom<String> for RedoPathBuf {
    type Error = RedoPathError;

    /// Coerces a UTF-8 string into a `RedoPathBuf`.
    ///
    /// This conversion does not allocate or copy memory.
    ///
    /// # Errors
    ///
    /// If the string contains any nul bytes, an error variant will be returned.
    fn try_from(s: String) -> Result<RedoPathBuf, RedoPathError> {
        if RedoPath::validate(&s) {
            Ok(unsafe { RedoPathBuf::from_string_unchecked(s) })
        } else {
            Err(RedoPathError { input: s.into() })
        }
    }
}

impl TryFrom<OsString> for RedoPathBuf {
    type Error = RedoPathError;

    /// Coerces a platform-native string into a `RedoPathBuf`.
    ///
    /// This conversion does not allocate or copy memory.
    ///
    /// # Errors
    ///
    /// If the string contains any nul bytes or is not valid UTF-8, an error
    /// variant will be returned.
    fn try_from(s: OsString) -> Result<RedoPathBuf, RedoPathError> {
        match s.into_string() {
            Ok(s) => RedoPathBuf::try_from(s),
            Err(input) => Err(RedoPathError { input }),
        }
    }
}

impl TryFrom<PathBuf> for RedoPathBuf {
    type Error = RedoPathError;

    #[inline]
    fn try_from(s: PathBuf) -> Result<RedoPathBuf, RedoPathError> {
        TryFrom::try_from(s.into_os_string())
    }
}

impl Deref for RedoPathBuf {
    type Target = RedoPath;

    #[inline]
    fn deref(&self) -> &Self::Target {
        self.as_redo_path()
    }
}

impl AsRef<RedoPath> for RedoPathBuf {
    #[inline]
    fn as_ref(&self) -> &RedoPath {
        unsafe { RedoPath::from_os_str_unchecked(self.as_os_str()) }
    }
}

impl AsRef<RedoPathBuf> for RedoPathBuf {
    #[inline]
    fn as_ref(&self) -> &RedoPathBuf {
        self
    }
}

impl AsRef<OsStr> for RedoPathBuf {
    #[inline]
    fn as_ref(&self) -> &OsStr {
        self.as_os_str()
    }
}

impl AsRef<Path> for RedoPathBuf {
    #[inline]
    fn as_ref(&self) -> &Path {
        Path::new(self.as_os_str())
    }
}

impl AsRef<str> for RedoPathBuf {
    #[inline]
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl From<RedoPathBuf> for OsString {
    #[inline]
    fn from(p: RedoPathBuf) -> OsString {
        p.into_os_string()
    }
}

impl From<RedoPathBuf> for PathBuf {
    #[inline]
    fn from(p: RedoPathBuf) -> PathBuf {
        p.into_path_buf()
    }
}

impl From<RedoPathBuf> for String {
    #[inline]
    fn from(p: RedoPathBuf) -> String {
        p.into_string()
    }
}

impl<'a> From<&'a RedoPath> for RedoPathBuf {
    #[inline]
    fn from(p: &'a RedoPath) -> RedoPathBuf {
        p.to_redo_path_buf()
    }
}

impl FromStr for RedoPathBuf {
    type Err = RedoPathError;

    #[inline]
    fn from_str(s: &str) -> Result<RedoPathBuf, RedoPathError> {
        RedoPath::from_str(s).map(RedoPath::to_redo_path_buf)
    }
}

impl ToSql for RedoPathBuf {
    fn to_sql(&self) -> Result<ToSqlOutput, rusqlite::Error> {
        self.as_str().to_sql()
    }
}

impl FromSql for RedoPathBuf {
    fn column_result(value: rusqlite::types::ValueRef<'_>) -> Result<Self, FromSqlError> {
        use std::convert::TryInto;
        let s: String = FromSql::column_result(value)?;
        s.try_into().map_err(|e| FromSqlError::Other(Box::new(e)))
    }
}

impl<'a> From<Cow<'a, RedoPath>> for RedoPathBuf {
    #[inline]
    fn from(c: Cow<'a, RedoPath>) -> RedoPathBuf {
        c.into_owned()
    }
}

impl<'a> From<RedoPathBuf> for Cow<'a, RedoPath> {
    #[inline]
    fn from(p: RedoPathBuf) -> Cow<'a, RedoPath> {
        Cow::Owned(p)
    }
}

impl Borrow<RedoPath> for RedoPathBuf {
    #[inline]
    fn borrow(&self) -> &RedoPath {
        &*self
    }
}

impl Debug for RedoPathBuf {
    #[inline]
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        Debug::fmt(self.as_str(), f)
    }
}

impl Display for RedoPathBuf {
    #[inline]
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl NixPath for RedoPathBuf {
    fn is_empty(&self) -> bool {
        RedoPath::is_empty(self)
    }

    fn len(&self) -> usize {
        self.as_str().as_bytes().len()
    }

    fn with_nix_path<T, F>(&self, f: F) -> Result<T, nix::Error>
    where
        F: FnOnce(&CStr) -> T,
    {
        let cs = self.to_c_string();
        Ok(f(&cs))
    }
}

#[derive(Clone, Debug)]
pub struct RedoPathError {
    input: OsString,
}

impl RedoPathError {
    pub(crate) fn input(&self) -> &OsStr {
        &self.input
    }
}

impl Display for RedoPathError {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "could not use {:?} as redo path", self.input())
    }
}

impl std::error::Error for RedoPathError {}

impl From<RedoPathError> for RedoError {
    fn from(e: RedoPathError) -> RedoError {
        let msg = e.to_string();
        let input = e.input.clone();
        RedoError::wrap(e, msg).with_kind(RedoErrorKind::InvalidTarget(input))
    }
}

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
        Err(Errno::ENOENT) => Ok(()),
        res => res,
    }
}

/// Make a path absolute if it isn't already.
pub fn abs_path<'p, 'q, P, Q>(cwd: &'p P, path: &'q Q) -> Cow<'q, Path>
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
        tv_sec: d.as_secs() as time_t,
        tv_usec: d.subsec_micros() as suseconds_t,
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

#[cfg(any(target_os = "linux", target_os = "macos", target_os = "freebsd"))]
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
        let mut buf = OsString::from(original.as_os_str());
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
    pub(crate) fn osstr_slice(&self, n: usize) -> Cow<'a, OsStr> {
        use std::os::unix::ffi::OsStrExt;
        Cow::Borrowed(OsStr::from_bytes(&self.s.as_bytes()[..n]))
    }
}

#[cfg(unix)]
impl<'a> From<OsBytes<'a>> for &'a OsStr {
    fn from(b: OsBytes<'a>) -> &'a OsStr {
        b.s
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
            Some(mut buf) => Cow::Owned(OsBytes::new_osstring(if self.vol_len == 0 {
                buf.truncate(self.w);
                buf
            } else {
                let mut new_buf = Vec::with_capacity(self.vol_len + buf.len());
                new_buf.extend(OsBytes::new(&self.vol_and_path).take(self.vol_len));
                new_buf.extend(&buf[..self.w]);
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
        normpath_combo4: ("/abc/def/ghi/../../jkl/mno/..", "/abc/jkl"),
    );
}
