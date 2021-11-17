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

use lazy_static::lazy_static;
use libc::pid_t;
use nix::unistd::{self, Pid};
use std::env;
use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::io::{self, Write};
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::SystemTime;

use super::env::{Env, OptionalBool};

/// A line-based logger.
trait Logger {
    /// Write a line to the logger.
    /// `line` must not contain any `'\n'` characters.
    fn write_line(&mut self, line: &str);
}

/// A log printer for machine-readable logs, suitable for redo-log.
#[derive(Debug)]
struct RawLog<W> {
    file: W,
}

impl<W: Write> RawLog<W> {
    fn new(file: W) -> RawLog<W> {
        RawLog { file }
    }
}

impl<W: Write> Logger for RawLog<W> {
    fn write_line(&mut self, line: &str) {
        debug_assert!(!line.contains('\n'));

        // TODO(maybe): Prevent allocating on every write_line.
        let mut msg_with_nl = String::with_capacity(line.len() + 1);
        msg_with_nl.push_str(line);
        msg_with_nl.push('\n');

        let _ = io::stdout().flush();
        let _ = io::stderr().flush();
        let _ = self.file.write(msg_with_nl.as_bytes());
        let _ = self.file.flush();
    }
}

#[derive(Debug)]
struct PrettyLog<W> {
    file: W,
    topdir: PathBuf,
    escapes: ColorEscapes,
    config: PrettyLogConfig,
}

impl<W> PrettyLog<W> {
    fn new(file: W, escapes: ColorEscapes, config: PrettyLogConfig) -> PrettyLog<W> {
        PrettyLog {
            file,
            topdir: env::current_dir().expect("getcwd failed"),
            escapes,
            config,
        }
    }

    fn pretty(&self, buf: &mut Vec<u8>, pid: pid_t, color: &[u8], s: &str) {
        buf.extend(color);
        if self.config.debug_pids {
            let _ = write!(buf, "{:<6} redo  ", pid);
        } else {
            buf.extend(b"redo  ");
        }
        buf.extend((0..DEPTH.load(Ordering::SeqCst)).map(|_| b' '));
        if !color.is_empty() {
            buf.extend(self.escapes.bold);
        }
        buf.extend(s.as_bytes());
        buf.extend(self.escapes.plain);
        buf.push(b'\n')
    }
}

impl<W: Write> Logger for PrettyLog<W> {
    fn write_line(&mut self, line: &str) {
        debug_assert!(!line.contains('\n'));

        let mut buf: Vec<u8> = Vec::with_capacity(line.len() + 1);

        let _ = io::stdout().flush();
        let _ = io::stderr().flush();
        let meta: Option<(usize, Meta)> = line
            .find(Meta::PREFIX)
            .and_then(|start| Meta::parse(&line[start..]).ok().map(|meta| (start, meta)));
        match meta {
            Some((start, meta)) => {
                let _ = self.file.write(line[..start].as_bytes());
                match meta.kind {
                    "unchanged" => {
                        if self.config.log.unwrap_or(true) || self.config.debug != 0 {
                            self.pretty(
                                &mut buf,
                                meta.pid,
                                b"",
                                &format!("{} (unchanged)", meta.text),
                            );
                        }
                    }
                    "check" => self.pretty(
                        &mut buf,
                        meta.pid,
                        self.escapes.green,
                        &format!("({})", meta.text),
                    ),
                    "do" => self.pretty(&mut buf, meta.pid, self.escapes.green, meta.text),
                    "done" => {
                        if let Some((rv, name)) = Meta::parse_done_text(meta.text) {
                            if rv != 0 {
                                self.pretty(
                                    &mut buf,
                                    meta.pid,
                                    self.escapes.red,
                                    &format!("{} (exit {})", name, rv),
                                );
                            } else if self.config.verbose > 0
                                || self.config.xtrace > 0
                                || self.config.debug > 0
                            {
                                self.pretty(
                                    &mut buf,
                                    meta.pid,
                                    self.escapes.green,
                                    &format!("{} (done)", name),
                                );
                                buf.push(b'\n');
                            }
                        } else {
                            // Could not parse. Pass text through.
                            self.pretty(&mut buf, meta.pid, b"", meta.text);
                        }
                    }
                    "resumed" => self.pretty(
                        &mut buf,
                        meta.pid,
                        self.escapes.green,
                        &format!("{} (resumed)", meta.text),
                    ),
                    "locked" => {
                        if self.config.debug_locks {
                            self.pretty(
                                &mut buf,
                                meta.pid,
                                self.escapes.green,
                                &format!("{} (locked...)", meta.text),
                            );
                        }
                    }
                    "waiting" => {
                        if self.config.debug_locks {
                            self.pretty(
                                &mut buf,
                                meta.pid,
                                self.escapes.green,
                                &format!("{} (WAITING)", meta.text),
                            );
                        }
                    }
                    "unlocked" => {
                        if self.config.debug_locks {
                            self.pretty(
                                &mut buf,
                                meta.pid,
                                self.escapes.green,
                                &format!("{} (...unlocked!)", meta.text),
                            );
                        }
                    }
                    "error" => {
                        buf.extend(self.escapes.red);
                        buf.extend(b"redo: ");
                        buf.extend(self.escapes.bold);
                        buf.extend(meta.text.as_bytes());
                        buf.extend(self.escapes.plain);
                        buf.push(b'\n');
                    }
                    "warning" => {
                        buf.extend(self.escapes.yellow);
                        buf.extend(b"redo: ");
                        buf.extend(self.escapes.bold);
                        buf.extend(meta.text.as_bytes());
                        buf.extend(self.escapes.plain);
                        buf.push(b'\n');
                    }
                    "debug" => self.pretty(&mut buf, meta.pid, b"", meta.text),
                    _ => {
                        // Unknown kind. Pass it through.
                        self.pretty(&mut buf, meta.pid, b"", meta.text);
                    }
                }
            }
            None => {
                buf.extend(line.as_bytes());
                buf.push(b'\n');
            }
        }
        if !buf.is_empty() {
            let _ = self.file.write(&buf);
        }
        let _ = self.file.flush();
    }
}

#[derive(Clone, Debug, Default)]
struct PrettyLogConfig {
    debug: i32,
    debug_locks: bool,
    debug_pids: bool,
    verbose: i32,
    xtrace: i32,
    log: OptionalBool,
}

impl From<&Env> for PrettyLogConfig {
    fn from(e: &Env) -> PrettyLogConfig {
        PrettyLogConfig {
            debug: e.debug,
            debug_locks: e.debug_locks(),
            debug_pids: e.debug_pids(),
            verbose: e.verbose,
            xtrace: e.xtrace,
            log: e.log(),
        }
    }
}

lazy_static! {
    static ref GLOBAL_LOGGER: Mutex<Option<Box<dyn Logger + Send>>> = Mutex::new(None);
}

/// A builder used for setting up logs.
#[derive(Clone, Debug)]
pub struct LogBuilder {
    parent_logs: bool,
    pretty: bool,
    color: OptionalBool,
}

impl LogBuilder {
    #[inline]
    pub fn new() -> LogBuilder {
        LogBuilder {
            parent_logs: false,
            pretty: true,
            color: OptionalBool::Auto,
        }
    }

    /// Signal whether the parent process logs.
    #[inline]
    pub fn parent_logs(&mut self, val: bool) -> &mut Self {
        self.parent_logs = val;
        self
    }

    /// Set whether logs should be pretty-printed.
    #[inline]
    pub fn pretty(&mut self, val: bool) -> &mut Self {
        self.pretty = val;
        self
    }

    /// Sets terminal color behavior.
    #[inline]
    pub fn color(&mut self, val: OptionalBool) -> &mut Self {
        self.color = val;
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

    /// Set up the process-wide logger with the builder's settings.
    pub fn setup<W: WriteWithMaybeFd + Send + 'static>(&self, env: &Env, tty: W) {
        let logger: Box<dyn Logger + Send> = if self.pretty && !self.parent_logs {
            let escapes = tty
                .as_raw_fd()
                .map(|fd| check_tty(fd, self.color))
                .unwrap_or_default();
            Box::new(PrettyLog::new(tty, escapes, env.into()))
        } else {
            Box::new(RawLog::new(tty))
        };

        DEBUG_LEVEL.store(env.debug, Ordering::SeqCst);
        set_depth(env.depth().len());
        {
            let mut global_logger = GLOBAL_LOGGER.lock().unwrap();
            *global_logger = Some(logger);
        }
    }
}

impl Default for LogBuilder {
    #[inline]
    fn default() -> LogBuilder {
        LogBuilder::new()
    }
}

impl From<&Env> for LogBuilder {
    fn from(e: &Env) -> LogBuilder {
        LogBuilder {
            parent_logs: e.log().unwrap_or(true),
            pretty: e.pretty().unwrap_or(true),
            color: e.color(),
        }
    }
}

/// A wrapper over the [`Write`] trait that can optionally capture
/// the raw file descriptor.
pub trait WriteWithMaybeFd: Write {
    fn as_raw_fd(&self) -> Option<RawFd> {
        None
    }
}

impl<T: Write + AsRawFd> WriteWithMaybeFd for T {
    fn as_raw_fd(&self) -> Option<RawFd> {
        Some(AsRawFd::as_raw_fd(self))
    }
}

/// Global debug level (used for `log_*` macros).
///
/// Defaults to 3 before set up to help debug early startup.
static DEBUG_LEVEL: AtomicI32 = AtomicI32::new(3);

/// Return the currently configured global debug level.
#[inline]
pub fn debug_level() -> i32 {
    DEBUG_LEVEL.load(Ordering::SeqCst)
}

static DEPTH: AtomicUsize = AtomicUsize::new(0);

/// Reduces the depth of subsequent entries written to the global logger by 2.
/// It returns the previous depth value.
pub fn reduce_depth() -> usize {
    let mut old = DEPTH.load(Ordering::SeqCst);
    loop {
        // Avoid wrapping.
        let new = if old > 2 { old - 2 } else { 0 };
        match DEPTH.compare_exchange(old, new, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return old,
            Err(curr) => old = curr,
        }
    }
}

/// Sets the depth of subsequent entries written to the global logger.
#[inline]
pub fn set_depth(depth: usize) {
    DEPTH.store(depth, Ordering::SeqCst);
}

/// Write a line to the process-wide logger.
///
/// If this is called before [`setup`], then the line is written to stderr.
///
/// # Panics
///
/// If `s` contains a `'\n'` character.
pub fn write(line: &str) {
    assert!(!line.contains('\n'));
    {
        let mut logger = GLOBAL_LOGGER
            .lock()
            .expect("previous call to logger failed");
        let logger: &mut Option<Box<dyn Logger + Send>> = &mut *logger;
        if let Some(logger) = logger {
            logger.write_line(line);
            return;
        }
    }
    // Fallback to stderr, showing everything.
    // This is helpful for debugging early startup.
    let escapes = check_tty(AsRawFd::as_raw_fd(&io::stderr()), OptionalBool::Off);
    let mut logger = PrettyLog::new(
        io::stderr(),
        escapes,
        PrettyLogConfig {
            debug: 3,
            debug_locks: true,
            debug_pids: true,
            ..PrettyLogConfig::default()
        },
    );
    logger.write_line(line);
}

/// Write a structured log-line to the process-wide logger.
pub fn meta(kind: &str, s: &str, pid: Option<Pid>) {
    let now = SystemTime::now();
    let timestamp = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("system time before UNIX epoch");
    debug_assert!(!kind.contains(':'));
    debug_assert!(!kind.contains('@'));
    let pid = pid.unwrap_or(unistd::getpid());
    let meta = Meta {
        kind,
        pid: pid.as_raw(),
        timestamp: timestamp.as_secs_f64(),
        text: s,
    };
    write(&format!("{}", meta));
}

/// An immutable reference to a structured log-line.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Meta<'a> {
    kind: &'a str,
    pid: pid_t,
    timestamp: f64,
    text: &'a str,
}

impl<'a> Meta<'a> {
    const PREFIX: &'static str = "@@REDO:";
    const SEP: &'static str = "@@ ";

    /// Parses a log-line in-place.
    pub fn parse(s: &'a str) -> Result<Meta<'a>, MetaParseError> {
        if !s.starts_with(Meta::PREFIX) {
            return Err(MetaParseError::new(format!(
                "{:?} does not begin with {:?}",
                s,
                Meta::PREFIX
            )));
        }
        if s.contains('\n') {
            return Err(MetaParseError::new(format!("{:?} contains a newline", s)));
        }
        let (_, meta_and_text) = s.split_at(Meta::PREFIX.len());
        let meta_end = match meta_and_text.find(Meta::SEP) {
            Some(i) => i,
            None => {
                return Err(MetaParseError::new(format!(
                    "{:?} has unterminated metadata",
                    s
                )))
            }
        };
        let (meta, text) = {
            let (meta, line_with_sep) = meta_and_text.split_at(meta_end);
            let (_, line) = line_with_sep.split_at(Meta::SEP.len());
            (meta, line)
        };
        if meta.contains('@') {
            return Err(MetaParseError::new(format!(
                "{:?} contains @ inside metadata",
                s
            )));
        }
        let mut words = meta.split(':');
        let kind = words.next().unwrap();
        let pid = match words.next() {
            Some(pid) => str::parse::<pid_t>(pid)
                .map_err(|_| MetaParseError::new(format!("cannot parse pid in {:?}", s)))?,
            None => return Err(MetaParseError::new(format!("{:?} is missing a pid", s))),
        };
        let timestamp = match words.next() {
            Some(timestamp) => str::parse::<f64>(timestamp)
                .map_err(|_| MetaParseError::new(format!("cannot parse timestamp in {:?}", s)))?,
            None => {
                return Err(MetaParseError::new(format!(
                    "{:?} is missing a timestamp",
                    s
                )))
            }
        };
        // TODO(maybe): Store additional fields?

        Ok(Meta {
            kind,
            pid,
            timestamp,
            text,
        })
    }

    #[inline]
    pub fn kind(self) -> &'a str {
        self.kind
    }

    #[inline]
    pub fn pid(self) -> Pid {
        Pid::from_raw(self.pid)
    }

    #[inline]
    pub fn timestamp(self) -> f64 {
        self.timestamp
    }

    #[inline]
    pub fn text(self) -> &'a str {
        self.text
    }

    /// Parses a `"done"`-kind line into its exit code and the target name.
    /// Returns `None` if `kind != "done"` or the text is not of the right format.
    pub fn done_text(self) -> Option<(i32, &'a str)> {
        if self.kind == "done" {
            Meta::parse_done_text(self.text)
        } else {
            None
        }
    }

    fn parse_done_text<'b>(text: &'b str) -> Option<(i32, &'b str)> {
        let i = match text.find(' ') {
            Some(i) => i,
            None => return None,
        };
        let (rv, name_plus_space) = text.split_at(i);
        let rv = match str::parse::<i32>(rv) {
            Ok(rv) => rv,
            Err(_) => return None,
        };
        Some((rv, &name_plus_space[1..]))
    }
}

impl<'a> Display for Meta<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), fmt::Error> {
        write!(
            f,
            "{}{}:{}:{:.4}{}{}",
            Meta::PREFIX,
            self.kind,
            self.pid,
            self.timestamp,
            Meta::SEP,
            self.text
        )
    }
}

/// The error type returned from [`Meta::parse`].
#[derive(Debug)]
pub struct MetaParseError {
    msg: String,
}

impl MetaParseError {
    #[inline]
    fn new(msg: String) -> Self {
        MetaParseError { msg }
    }
}

impl Display for MetaParseError {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        Display::fmt(&self.msg, f)
    }
}

impl Error for MetaParseError {}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ColorEscapes {
    red: &'static [u8],
    green: &'static [u8],
    yellow: &'static [u8],
    bold: &'static [u8],
    plain: &'static [u8],
}

impl Default for ColorEscapes {
    /// Returns an empty set of color escapes.
    fn default() -> ColorEscapes {
        let zero = b"";
        ColorEscapes {
            red: zero,
            green: zero,
            yellow: zero,
            bold: zero,
            plain: zero,
        }
    }
}

fn check_tty(tty: RawFd, color: OptionalBool) -> ColorEscapes {
    let color = color.unwrap_or_else(|| {
        unistd::isatty(tty).unwrap_or(false)
            && env::var_os("TERM").map_or(false, |v| v != "dumb" && v != "")
    });
    if color {
        ColorEscapes {
            red: b"\x1b[31m",
            green: b"\x1b[32m",
            yellow: b"\x1b[33m",
            bold: b"\x1b[1m",
            plain: b"\x1b[m",
        }
    } else {
        ColorEscapes::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_done_text_simple() {
        assert_eq!(Meta::parse_done_text("0 foo"), Some((0, "foo")));
    }

    #[test]
    fn parse_done_text_multiple_spaces() {
        assert_eq!(Meta::parse_done_text("1 foo bar"), Some((1, "foo bar")));
    }
}
