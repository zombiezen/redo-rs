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
mod cycles;
mod deps;
mod env;
mod error;
mod helpers;
mod jobserver;
pub mod logs;
mod paths;
mod state;

pub use deps::{is_dirty, Dirtiness, DirtyCallbacks, DirtyCallbacksBuilder};
pub use env::{Env, OptionalBool};
pub use error::{RedoError, RedoErrorKind};
pub use helpers::{abs_path, normpath, RedoPath, RedoPathBuf};
pub use jobserver::JobServer;
pub use paths::{possible_do_files, DoFile, PossibleDoFiles};
pub use state::{
    always_filename, logname, relpath, DepMode, File, FileError, FileErrorKind, Files, Lock,
    LockType, ProcessState, ProcessTransaction, Stamp, LOG_LOCK_MAGIC,
};
