use common_path;
use failure::{format_err, Error};
use std::borrow::Cow;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fmt::{self, Display, Formatter};
use std::fs;
use std::iter;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use super::helpers;

#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct Env {
    is_toplevel: bool,
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
    log: i32,
    pub(crate) log_inode: OsString,
    color: i32,
    pretty: i32,
    pub(crate) shuffle: bool,
    pub(crate) startdir: PathBuf,
    pub(crate) runid: Option<i64>,
    pub(crate) unlocked: bool,
    pub(crate) no_oob: bool,
}

impl Env {
    /// Start a session (if needed) for a command that does need the state db.
    pub fn init<T: AsRef<str>>(targets: &[T]) -> Result<Env, Error> {
        let mut is_toplevel = false;
        if !get_bool("REDO") {
            is_toplevel = true;
            let exe_path = env::current_exe()?;
            let exe_names = [&exe_path, &fs::canonicalize(&exe_path)?];
            let dir_names: Vec<&Path> = exe_names.iter().filter_map(|&p| p.parent()).collect();
            let mut try_names: Vec<Cow<Path>> = Vec::new();
            try_names.extend(dir_names.iter().map(|&p| {
                let mut p2 = PathBuf::from(p);
                p2.extend(["..", "lib", "redo"].iter());
                Cow::Owned(p2)
            }));
            try_names.extend(dir_names.iter().map(|&p| {
                let mut p2 = PathBuf::from(p);
                p2.extend(["..", "redo"].iter());
                Cow::Owned(p2)
            }));
            try_names.extend(dir_names.iter().map(|&p| Cow::Borrowed(p)));

            let mut dirs: Vec<Cow<Path>> = Vec::new();
            for k in try_names {
                if !dirs.iter().any(|k2| k2 == &k) {
                    dirs.push(k);
                }
            }
            let old_path = env::var_os("PATH").unwrap_or_default();
            let mut new_path = OsString::new();
            for p in dirs {
                new_path.push(p.as_os_str());
                new_path.push(":");
            }
            new_path.push(old_path);
            env::set_var("PATH", new_path);
            env::set_var("REDO", exe_path);
        }
        if !get_bool("REDO_BASE") {
            let targets: Vec<&str> = if targets.is_empty() {
                // If no other targets given, assume the current directory.
                vec!["all"]
            } else {
                targets.iter().map(AsRef::as_ref).collect()
            };
            let cwd = env::current_dir()?;
            let maybe_dirs: Vec<Option<PathBuf>> = targets
                .iter()
                .map(|t| {
                    Path::new(t)
                        .parent()
                        .map(|par| helpers::abs_path(&cwd, &par).into_owned())
                })
                .collect();
            if maybe_dirs.iter().any(|o| o.is_none()) {
                return Err(format_err!("invalid targets"));
            }
            let orig_base = common_path::common_path_all(
                maybe_dirs
                    .iter()
                    .map(|o| o.as_ref().unwrap().as_ref())
                    .chain(iter::once(cwd.as_ref())),
            )
            .unwrap();
            let mut base = Some(orig_base.clone());
            while let Some(mut b) = base {
                b.push(".redo");
                if b.exists() {
                    base = Some(b);
                    break;
                }
                b.pop(); // .redo
                base = if b.pop() {
                    // up to parent
                    None
                } else {
                    Some(b)
                };
            }
            env::set_var("REDO_BASE", base.unwrap_or(orig_base));
            env::set_var("REDO_STARTDIR", cwd);
        }
        Ok(Env {
            is_toplevel,
            ..Env::inherit()?
        })
    }

    /// Start a session (if needed) for a command that needs no state db.
    pub fn init_no_state() -> Result<Env, Error> {
        let mut is_toplevel = false;
        if !get_bool("REDO") {
            env::set_var("REDO", "NOT_DEFINED");
            is_toplevel = true;
        }
        if !get_bool("REDO_BASE") {
            env::set_var("REDO_BASE", "NOT_DEFINED");
        }
        Ok(Env {
            is_toplevel,
            ..Env::inherit()?
        })
    }

    /// Read environment (which must already be set) to get runtime settings.
    pub fn inherit() -> Result<Env, Error> {
        if !get_bool("REDO") {
            return Err(format_err!("must be run from inside a .do"));
        }
        let v = Env {
            is_toplevel: false,
            base: env::var_os("REDO_BASE").unwrap_or_default().into(),
            pwd: env::var_os("REDO_PWD").unwrap_or_default().into(),
            target: env::var_os("REDO_TARGET").unwrap_or_default().into(),
            depth: env::var_os("REDO_DEPTH").unwrap_or_default(),
            debug: get_int("REDO_DEBUG", 0) as i32,
            debug_locks: get_bool("REDO_DEBUG_LOCKS"),
            debug_pids: get_bool("REDO_DEBUG_PIDS"),
            locks_broken: get_bool("REDO_LOCKS_BROKEN"),
            verbose: get_int("REDO_VERBOSE", 0) as i32,
            xtrace: get_int("REDO_XTRACE", 0) as i32,
            keep_going: get_bool("REDO_KEEP_GOING"),
            log: get_int("REDO_LOG", 1) as i32,
            log_inode: env::var_os("REDO_LOG_INODE").unwrap_or_default(),
            color: get_int("REDO_COLOR", 0) as i32,
            pretty: get_int("REDO_PRETTY", 0) as i32,
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
        Ok(v)
    }

    #[inline]
    pub fn is_toplevel(&self) -> bool {
        self.is_toplevel
    }

    #[inline]
    pub fn pwd(&self) -> &Path {
        &self.pwd
    }

    #[inline]
    pub fn target(&self) -> &Path {
        &self.target
    }

    #[inline]
    pub fn locks_broken(&self) -> bool {
        self.locks_broken
    }

    #[inline]
    pub fn log(&self) -> OptionalBool {
        if self.log == 0 {
            OptionalBool::Off
        } else if self.log == 1 {
            OptionalBool::Auto
        } else {
            OptionalBool::On
        }
    }

    #[inline]
    pub fn color(&self) -> OptionalBool {
        if self.color == 0 {
            OptionalBool::Off
        } else if self.color == 1 {
            OptionalBool::Auto
        } else {
            OptionalBool::On
        }
    }

    #[inline]
    pub fn pretty(&self) -> OptionalBool {
        if self.pretty == 0 {
            OptionalBool::Off
        } else if self.pretty == 1 {
            OptionalBool::Auto
        } else {
            OptionalBool::On
        }
    }

    #[inline]
    pub fn startdir(&self) -> &Path {
        &self.startdir
    }

    #[inline]
    pub fn is_unlocked(&self) -> bool {
        self.unlocked
    }

    /// If file locking is broken, update the environment accordingly.
    pub(crate) fn mark_locks_broken(&mut self) {
        env::set_var("REDO_LOCKS_BROKEN", "1");
        // FIXME: redo-log doesn't work when fcntl locks are broken.
        // We can probably work around that someday.
        env::set_var("REDO_LOG", "0");

        self.locks_broken = true;
        self.log = 0;
    }

    pub(crate) fn fill_runid(&mut self, runid: i64) {
        assert!(self.runid.is_none());
        self.runid = Some(runid);
        env::set_var("REDO_RUNID", runid.to_string());
    }
}

fn get_int<K: AsRef<OsStr>>(key: K, default: i64) -> i64 {
    env::var(key)
        .ok()
        .and_then(|v| i64::from_str(&v).ok())
        .unwrap_or(default)
}

fn get_bool<K: AsRef<OsStr>>(key: K) -> bool {
    env::var_os(key).map_or(false, |v| !v.is_empty())
}

/// A tri-state value that is forced on or off, or has an automatic (default) value.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
#[repr(u8)]
pub enum OptionalBool {
    Off = 0,
    Auto = 1,
    On = 2,
}

impl OptionalBool {
    /// Returns the boolean value or a provided default.
    #[inline]
    pub fn unwrap_or(self, default: bool) -> bool {
        match self {
            OptionalBool::On => true,
            OptionalBool::Off => false,
            OptionalBool::Auto => default,
        }
    }

    /// Returns the boolean value or computes it from a closure.
    #[inline]
    pub fn unwrap_or_else<F: FnOnce() -> bool>(self, f: F) -> bool {
        match self {
            OptionalBool::On => true,
            OptionalBool::Off => false,
            OptionalBool::Auto => f(),
        }
    }
}

impl Default for OptionalBool {
    #[inline]
    fn default() -> OptionalBool {
        OptionalBool::Auto
    }
}

impl Display for OptionalBool {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            OptionalBool::Off => f.write_str("false"),
            OptionalBool::Auto => f.write_str("auto"),
            OptionalBool::On => f.write_str("true"),
        }
    }
}

impl From<Option<bool>> for OptionalBool {
    fn from(ob: Option<bool>) -> OptionalBool {
        match ob {
            Some(true) => OptionalBool::On,
            Some(false) => OptionalBool::Off,
            None => OptionalBool::Auto,
        }
    }
}

impl From<OptionalBool> for Option<bool> {
    fn from(ob: OptionalBool) -> Option<bool> {
        match ob {
            OptionalBool::On => Some(true),
            OptionalBool::Off => Some(false),
            OptionalBool::Auto => None,
        }
    }
}
