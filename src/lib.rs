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
mod env;
mod helpers;
mod jobserver;
pub mod logs;
mod paths;
mod state;

pub use env::Env;
pub use jobserver::JobServer;
pub use state::{DepMode, File, ProcessState, ProcessTransaction, Stamp, ALWAYS};

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
            eprintln!("{}: {}", name, msg);
            let retcode = e
                .downcast_ref::<builder::BuildError>()
                .map(|be| i32::from(be.kind()))
                .unwrap_or(1);
            std::process::exit(retcode)
        }
    }
}
