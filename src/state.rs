use failure::{format_err, Error, ResultExt};
use libc::{self, c_int, c_short, flock, off_t};
use nix;
use nix::errno::Errno;
use nix::fcntl::{self, FcntlArg};
use nix::sys::wait::{self, WaitStatus};
use nix::unistd::{self, ForkResult};
use rusqlite::{self, params, Connection, OptionalExtension, NO_PARAMS};
use std::cell::RefCell;
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::mem;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process;
use std::time::Duration;

use super::env::{self, Env};
use super::helpers;

const SCHEMA_VER: i32 = 2;

const ALWAYS: &str = "//ALWAYS";

#[derive(Debug)]
#[non_exhaustive]
pub(crate) struct ProcessState {
    db: Connection,
    lock_manager: LockManager,
    env: Env,
    is_toplevel: bool,
}

impl ProcessState {
    pub(crate) fn init<T: AsRef<str>>(targets: &[T]) -> Result<ProcessState, Error> {
        let (mut e, is_toplevel) = env::init(targets)?;
        let dbdir = {
            let mut dbdir = PathBuf::from(&e.base);
            dbdir.push(".redo");
            dbdir
        };
        if let Err(err) = fs::create_dir(&dbdir) {
            if err.kind() != io::ErrorKind::AlreadyExists {
                return Err(Error::from(err)
                    .context("Could not create database directory")
                    .into());
            }
        }
        let lockfile = {
            let mut lockfile = PathBuf::from(&dbdir);
            lockfile.push("locks");
            lockfile
        };
        let lock_manager = LockManager::open(lockfile)?;
        if is_toplevel && lock_manager.detect_broken_locks()? {
            e.mark_locks_broken();
        }
        let dbfile = {
            let mut dbfile = PathBuf::from(&dbdir);
            dbfile.push("db.sqlite3");
            dbfile
        };
        let must_create = !dbfile.exists();
        let mut db: Connection;
        {
            let tx = if !must_create {
                db = connect(&e, &dbfile).with_context(|e| format!("could not connect: {}", e))?;
                let tx = db.transaction()?;
                let ver: Option<i32> = tx
                    .query_row("select version from Schema", NO_PARAMS, |row| row.get(0))
                    .optional()
                    .context("schema version check failed")?;
                if ver != Some(SCHEMA_VER) {
                    return Err(format_err!(
                        "{}: found v{} (expected v{})\nmanually delete .redo dir to start over.",
                        dbfile.to_string_lossy(),
                        ver.unwrap_or(0),
                        SCHEMA_VER
                    ));
                }
                tx
            } else {
                helpers::unlink(&dbfile)?;
                db = connect(&e, &dbfile).with_context(|e| format!("could not connect: {}", e))?;
                let tx = db.transaction()?;
                tx.execute(
                    "create table Schema \
                        (version int)",
                    NO_PARAMS,
                )
                .context("create table Schema")?;
                tx.execute(
                    "create table Runid \
                        (id integer primary key autoincrement)",
                    NO_PARAMS,
                )
                .context("create table Runid")?;
                tx.execute(
                    "create table Files \
                        (name not null primary key, \
                        is_generated int, \
                        is_override int, \
                        checked_runid int, \
                        changed_runid int, \
                        failed_runid int, \
                        stamp,
                        csum)",
                    NO_PARAMS,
                )
                .context("create table Files")?;
                tx.execute(
                    "create table Deps \
                        (target int, \
                        source int, \
                        mode not null, \
                        delete_me int, \
                        primary key (target, source))",
                    NO_PARAMS,
                )
                .context("create table Deps")?;
                tx.execute(
                    "insert into Schema (version) values (?)",
                    params![SCHEMA_VER],
                )
                .context("create table Schema")?;
                // eat the '0' runid and File id.
                // Because of the cheesy way t/flush-cache is implemented, leave a
                // lot of runids available before the "first" one so that we
                // can adjust cached values to be before the first value.
                tx.execute("insert into Runid values (1000000000)", NO_PARAMS)
                    .context("insert initial Runid")?;
                tx.execute("insert into Files (name) values (?)", params![ALWAYS])
                    .context("insert ALWAYS file")?;
                tx
            };

            if e.runid.is_none() {
                tx.execute(
                    "insert into Runid values \
                        ((select max(id)+1 from Runid))",
                    NO_PARAMS,
                )
                .context("insert into Runid")?;
                e.fill_runid(
                    tx.query_row("select last_insert_rowid()", NO_PARAMS, |row| row.get(0))
                        .context("read runid")?,
                );
            }

            tx.commit().context("Commit database setup")?;
        }

        Ok(ProcessState {
            db,
            lock_manager,
            env: e,
            is_toplevel,
        })
    }

    pub(crate) fn env(&self) -> &Env {
        &self.env
    }
}

fn connect<P: AsRef<Path>>(env: &Env, dbfile: P) -> rusqlite::Result<Connection> {
    let db = Connection::open(dbfile)?;
    db.busy_timeout(Duration::from_secs(60))?;
    db.execute("pragma synchronous = off", NO_PARAMS)?;
    // Some old/broken versions of pysqlite on MacOS work badly with journal
    // mode PERSIST.  But WAL fails on Windows WSL due to WSL's totally broken
    // locking.  On WSL, at least PERSIST works in single-threaded mode, so
    // if we're careful we can use it, more or less.
    db.query_row(
        if env.locks_broken {
            "pragma journal_mode = PERSIST"
        } else {
            "pragma journal_mode = WAL"
        },
        NO_PARAMS,
        |_| Ok(()),
    )?;
    Ok(db)
}

#[derive(Debug)]
pub(crate) struct LockManager {
    file: File,
    locks: RefCell<HashSet<i32>>,
}

impl LockManager {
    pub(crate) fn open<P: AsRef<Path>>(path: P) -> Result<LockManager, Error> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)?;
        helpers::close_on_exec(&file, true)?;
        Ok(LockManager {
            file,
            locks: RefCell::new(HashSet::new()),
        })
    }

    pub(crate) fn new(&self, fid: i32) -> Lock {
        let mut locks = self.locks.borrow_mut();
        assert!(locks.insert(fid));
        Lock {
            manager: self,
            owned: false,
            fid,
        }
    }

    /// Detect Windows WSL's completely broken `fcntl()` locks.
    ///
    /// Symptom: locking a file always returns success, even if other processes
    /// also think they have it locked. See
    /// https://github.com/Microsoft/WSL/issues/1927 for more details.
    ///
    /// Bug exists at least in WSL "4.4.0-17134-Microsoft #471-Microsoft".
    ///
    /// Returns `true` if broken, `false` otherwise.
    pub(crate) fn detect_broken_locks(&self) -> nix::Result<bool> {
        let mut pl = self.new(0);
        // We wait for the lock here, just in case others are doing
        // this test at the same time.
        pl.wait_lock(LockType::Exclusive)?;
        match unistd::fork() {
            Ok(ForkResult::Parent { child: pid }) => match wait::waitpid(pid, None) {
                Ok(WaitStatus::Exited(_, status)) => Ok(status != 0),
                Ok(_) => Ok(true),
                Err(e) => Err(e),
            },
            Ok(ForkResult::Child) => {
                // Doesn't actually unlock, since child process doesn't own it.
                let _ = pl.unlock();
                mem::drop(pl);
                let mut cl = self.new(0);
                // parent is holding lock, which should prevent us from getting it.
                match cl.try_lock() {
                    Ok(true) => {
                        // Got the lock? Yikes, the locking system is broken!
                        process::exit(1);
                    }
                    Ok(false) => {
                        // Failed to get the lock? Good, the parent owns it.
                        process::exit(0);
                    }
                    Err(_) => {
                        // Some other error occurred. Stay safe and report failure.
                        process::exit(1);
                    }
                }
            }
            Err(e) => Err(e),
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub(crate) enum LockType {
    Exclusive,
    Shared,
}

impl Default for LockType {
    fn default() -> LockType {
        LockType::Exclusive
    }
}

/// An object representing a lock on a redo target file.
#[derive(Debug)]
pub(crate) struct Lock<'a> {
    manager: &'a LockManager,
    owned: bool,
    fid: i32,
}

impl<'a> Lock<'a> {
    /// Check that this lock is in a sane state.
    pub(crate) fn check(&self) {
        assert!(!self.owned);
    }

    /// Non-blocking try to acquire our lock; returns true if it worked.
    pub(crate) fn try_lock(&mut self) -> nix::Result<bool> {
        self.check();
        assert!(!self.owned);
        let result = fcntl::fcntl(
            self.manager.file.as_raw_fd(),
            FcntlArg::F_SETLK(&fid_flock(libc::F_WRLCK, self.fid)),
        );
        match result {
            Ok(_) => {
                self.owned = true;
                Ok(true)
            }
            Err(nix::Error::Sys(Errno::EACCES)) | Err(nix::Error::Sys(Errno::EAGAIN)) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Try to acquire our lock, and wait if it's currently locked.
    pub(crate) fn wait_lock(&mut self, lock_type: LockType) -> nix::Result<()> {
        self.check();
        assert!(!self.owned);
        let fcntl_type = match lock_type {
            LockType::Exclusive => libc::F_WRLCK,
            LockType::Shared => libc::F_RDLCK,
        };
        fcntl::fcntl(
            self.manager.file.as_raw_fd(),
            FcntlArg::F_SETLKW(&fid_flock(fcntl_type, self.fid)),
        )?;
        self.owned = true;
        Ok(())
    }

    /// Release the lock, which we must currently own.
    pub(crate) fn unlock(&mut self) -> nix::Result<()> {
        assert!(self.owned, "can't unlock {} - we don't own it", self.fid);
        fcntl::fcntl(
            self.manager.file.as_raw_fd(),
            FcntlArg::F_SETLK(&fid_flock(libc::F_UNLCK, self.fid)),
        )?;
        self.owned = false;
        Ok(())
    }
}

impl<'a> Drop for Lock<'a> {
    fn drop(&mut self) {
        let mut locks = self.manager.locks.borrow_mut();
        locks.remove(&self.fid);
        if self.owned {
            let _ = self.unlock();
        }
    }
}

fn fid_flock(typ: c_int, fid: i32) -> flock {
    flock {
        l_type: typ as c_short,
        l_whence: libc::SEEK_SET as c_short,
        l_start: fid as off_t,
        l_len: 1,
        l_pid: 0,
    }
}
