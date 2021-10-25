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

/// Log an error.
///
/// # Example
///
/// ```no_run
/// # use redo::log_err;
/// # fn main() {
/// log_err!("{} has failed", "everything");
/// # }
/// ```
#[macro_export]
macro_rules! log_err {
    ($($arg:tt)*) => {{
        let s = format!($($arg)*);
        $crate::logs::meta("error", s.trim_end(), None);
    }}
}

/// Log a warning.
///
/// # Example
///
/// ```no_run
/// # use redo::log_warn;
/// # fn main() {
/// log_warn!("{} has failed", "something non-critical");
/// # }
/// ```
#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {{
        let s = format!($($arg)*);
        $crate::logs::meta("warning", s.trim_end(), None);
    }}
}

/// Log a debug message.
///
/// # Example
///
/// ```no_run
/// # use redo::log_debug;
/// # fn main() {
/// log_debug!("some details");
/// # }
/// ```
#[macro_export]
macro_rules! log_debug {
    ($($arg:tt)*) => {{
        if $crate::logs::debug_level() >= 1 {
            let s = format!($($arg)*);
            $crate::logs::meta("debug", s.trim_end(), None);
        }
    }}
}

/// Log a verbose debug message.
///
/// # Example
///
/// ```no_run
/// # use redo::log_debug2;
/// # fn main() {
/// log_debug2!("some verbose details");
/// # }
/// ```
#[macro_export]
macro_rules! log_debug2 {
    ($($arg:tt)*) => {{
        if $crate::logs::debug_level() >= 2 {
            let s = format!($($arg)*);
            $crate::logs::meta("debug", s.trim_end(), None);
        }
    }}
}

/// Log an extra verbose debug message.
///
/// # Example
///
/// ```no_run
/// # use redo::log_debug3;
/// # fn main() {
/// log_debug3!("extra verbose details");
/// # }
/// ```
#[macro_export]
macro_rules! log_debug3 {
    ($($arg:tt)*) => {{
        if $crate::logs::debug_level() >= 3 {
            let s = format!($($arg)*);
            $crate::logs::meta("debug", s.trim_end(), None);
        }
    }}
}

pub mod builder;
mod deps;
mod env;
mod helpers;
mod jobserver;
pub mod logs;
mod paths;
mod state;

pub use deps::{is_dirty, Dirtiness};
pub use env::{Env, OptionalBool};
pub use helpers::{abs_path, normpath};
pub use jobserver::JobServer;
pub use paths::{possible_do_files, DoFile, PossibleDoFiles};
pub use state::{
    logname, relpath, DepMode, File, FileError, FileErrorKind, Lock, LockType, ProcessState,
    ProcessTransaction, Stamp, ALWAYS, LOG_LOCK_MAGIC,
};

/// Basic main wrapper.
pub fn run_program<S: AsRef<str>, F: FnOnce() -> Result<(), failure::Error>>(name: S, run: F) -> ! {
    match run() {
        Ok(_) => std::process::exit(0),
        Err(e) => {
            let name: String = match std::env::current_exe() {
                Ok(exe) => exe
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_else(|| String::from(name.as_ref())),
                Err(_) => String::from(name.as_ref()),
            };
            if let Some(bt) = e
                .iter_chain()
                .filter_map(|f| f.backtrace().filter(|bt| !bt.is_empty()))
                .last()
            {
                eprint!("Backtrace:\n{}\n\n", bt);
            }
            let msg = e.iter_chain().fold(String::new(), |mut s, e| {
                use std::fmt::Write;
                if !s.is_empty() {
                    write!(s, ": ").unwrap();
                }
                write!(s, "{}", e).unwrap();
                s
            });
            log_err!("{}: {}", name, msg);
            let retcode = e
                .downcast_ref::<builder::BuildError>()
                .map(|be| i32::from(be.kind()))
                .unwrap_or(1);
            std::process::exit(retcode)
        }
    }
}

/// Return the common list of `redo-log` flags.
pub fn redo_log_flags() -> Vec<clap::Arg<'static, 'static>> {
    use clap::Arg;

    vec![
        Arg::from_usage("--no-details").help("only show 'redo' recursion trace, not build output"),
        Arg::from_usage("--details")
            .hidden(true)
            .overrides_with("no-details"),
        Arg::from_usage("--no-status")
            .help("don't display build summary line at the bottom of the screen"),
        Arg::from_usage("--status")
            .hidden(true)
            .overrides_with("no-status"),
        Arg::from_usage("--no-pretty")
            .help("don't pretty-print logs, show raw @@REDO output instead"),
        Arg::from_usage("--pretty")
            .hidden(true)
            .overrides_with("no-pretty"),
        Arg::from_usage("--no-color")
            .help("disable ANSI color; --color to force enable (default: auto)"),
        Arg::from_usage("--color")
            .hidden(true)
            .overrides_with("no-color"),
        Arg::from_usage("--debug-locks 'print messages about file locking (useful for debugging)'"),
        Arg::from_usage("--no-debug-locks")
            .hidden(true)
            .overrides_with("debug-locks"),
        Arg::from_usage(
            "--debug-pids 'print process ids as part of log messages (useful for debugging)'",
        ),
        Arg::from_usage("--no-debug-pids")
            .hidden(true)
            .overrides_with("debug-pids"),
    ]
}

/// Converts an argument pair match of `name` and `"no-" + name` into a tri-state.
pub fn auto_bool_arg<S: AsRef<str>>(matches: &clap::ArgMatches, name: S) -> OptionalBool {
    let name = name.as_ref();
    const NEGATIVE_PREFIX: &str = "no-";
    let negative_name = {
        let mut negative_name = String::with_capacity(name.len() + NEGATIVE_PREFIX.len());
        negative_name.push_str(NEGATIVE_PREFIX);
        negative_name.push_str(name);
        negative_name
    };
    if matches.is_present(name) {
        OptionalBool::On
    } else if matches.is_present(negative_name) {
        OptionalBool::Off
    } else {
        OptionalBool::Auto
    }
}
