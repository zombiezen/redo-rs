use std::env;
use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::str::FromStr;

#[derive(Debug)]
#[non_exhaustive]
pub(crate) struct Env {
  pub(crate) base: PathBuf,
  pub(crate) pwd: PathBuf,
  pub(crate) target: PathBuf,
  pub(crate) depth: OsString,
  pub(crate) debug: i32,
  pub(crate) debug_locks: bool,
  pub(crate) debug_pids: bool,
  pub(crate) locks_broken: bool,
  pub(crate) verbose: i32,
  pub(crate) xtrace: i32,
  pub(crate) keep_going: bool,
  pub(crate) log: i32,
  pub(crate) log_inode: OsString,
  pub(crate) color: i32,
  pub(crate) pretty: i32,
  pub(crate) shuffle: bool,
  pub(crate) startdir: PathBuf,
  pub(crate) runid: Option<i32>,
  pub(crate) unlocked: bool,
  pub(crate) no_oob: bool,
}

impl Env {
  /// Read environment (which must already be set) to get runtime settings.
  fn inherit() -> Env {
    let v = Env{
      base: env::var_os("REDO_BASE").unwrap_or_default().into(),
      pwd: env::var_os("REDO_PWD").unwrap_or_default().into(),
      target: env::var_os("REDO_TARGET").unwrap_or_default().into(),
      depth: env::var_os("REDO_DEPTH").unwrap_or_default(),
      debug: get_int("REDO_DEBUG", 0),
      debug_locks: get_bool("REDO_DEBUG_LOCKS"),
      debug_pids: get_bool("REDO_DEBUG_PIDS"),
      locks_broken: get_bool("REDO_LOCKS_BROKEN"),
      verbose: get_int("REDO_VERBOSE", 0),
      xtrace: get_int("REDO_XTRACE", 0),
      keep_going: get_bool("REDO_KEEP_GOING"),
      log: get_int("REDO_LOG", 1),
      log_inode: env::var_os("REDO_LOG_INODE").unwrap_or_default(),
      color: get_int("REDO_COLOR", 0),
      pretty: get_int("REDO_PRETTY", 0),
      shuffle: get_bool("REDO_SHUFFLE"),
      startdir: env::var_os("REDO_STARTDIR").unwrap_or_default().into(),
      runid: match get_int("REDO_RUNID", 0) {
        0 => None,
        x => Some(x),
      },
      unlocked: get_bool("REDO_UNLOCKED"),
      no_oob: get_bool("REDO_NO_OOB"),
    };
    // not inheritable by subprocesses
    env::set_var("REDO_UNLOCKED", "");
    env::set_var("REDO_NO_OOB", "");
    v
  }
}

/// Start a session (if needed) for a command that needs no state db.
pub(crate) fn init_no_state() -> (Env, bool) {
  let mut is_toplevel = false;
  if !get_bool("REDO") {
    env::set_var("REDO", "NOT_DEFINED");
    is_toplevel = true;
  }
  if !get_bool("REDO_BASE") {
    env::set_var("REDO_BASE", "NOT_DEFINED");
  }
  (Env::inherit(), is_toplevel)
}

/// Start a session (if needed) for a command that does need the state db.
pub(crate) fn init<T: AsRef<str>>(targets: &[T]) -> (Env, bool) {
  let mut is_toplevel = false;
  if !get_bool("REDO") {
    is_toplevel = true;
    // TODO(now)
  }
  if !get_bool("REDO_BASE") {
    // TODO(now)
  }
  (Env::inherit(), is_toplevel)
}

fn get_int<K: AsRef<OsStr>>(key: K, default: i32) -> i32 {
  env::var(key).ok().and_then(|v| i32::from_str(&v).ok()).unwrap_or(default)
}

fn get_bool<K: AsRef<OsStr>>(key: K) -> bool {
  env::var_os(key).map_or(false, |v| !v.is_empty())
}
