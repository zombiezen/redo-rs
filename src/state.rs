use failure::{format_err, Error, ResultExt};
use libc::{self, c_int, c_short, flock, off_t};
use libsqlite3_sys;
use nix;
use nix::errno::Errno;
use nix::fcntl::{self, FcntlArg};
use nix::sys::wait::{self, WaitStatus};
use nix::unistd::{self, ForkResult};
use rusqlite::{
    self, params, Connection, DropBehavior, OptionalExtension, Row, ToSql, TransactionBehavior,
    NO_PARAMS,
};
use std::borrow::Cow;
use std::cell::RefCell;
use std::cmp;
use std::collections::HashSet;
use std::fs::{self, OpenOptions};
use std::io;
use std::mem;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process;
use std::rc::Rc;
use std::time::Duration;

use super::env::{self, Env};
use super::helpers;

const SCHEMA_VER: i32 = 2;

const ALWAYS: &str = "//ALWAYS";

#[derive(Debug)]
#[non_exhaustive]
pub(crate) struct ProcessState {
    db: Connection,
    lock_manager: Rc<LockManager>,
    env: Env,
    is_toplevel: bool,
    wrote: i32,
}

#[derive(Debug)]
pub(crate) struct ProcessTransaction<'a> {
    state: &'a mut ProcessState,
    drop_behavior: DropBehavior,
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
        let lock_manager = Rc::new(LockManager::open(lockfile)?);
        if is_toplevel && LockManager::detect_broken_locks(&lock_manager)? {
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
            wrote: 0,
        })
    }

    #[inline]
    pub(crate) fn env(&self) -> &Env {
        &self.env
    }

    #[inline]
    pub(crate) fn new_lock(&self, fid: i32) -> Lock {
        Lock::new(self.lock_manager.clone(), fid)
    }

    #[inline]
    pub(crate) fn is_toplevel(&self) -> bool {
        self.is_toplevel
    }

    #[inline]
    pub(crate) fn is_flushed(&self) -> bool {
        self.wrote == 0
    }

    fn write<P>(&mut self, sql: &str, params: P) -> rusqlite::Result<usize>
    where
        P: IntoIterator,
        P::Item: ToSql,
    {
        self.wrote += 1;
        self.db.execute(sql, params)
    }
}

impl<'a> ProcessTransaction<'a> {
    pub(crate) fn new(
        state: &'a mut ProcessState,
        behavior: TransactionBehavior,
    ) -> rusqlite::Result<ProcessTransaction> {
        let query = match behavior {
            TransactionBehavior::Deferred => "BEGIN DEFERRED",
            TransactionBehavior::Immediate => "BEGIN IMMEDIATE",
            TransactionBehavior::Exclusive => "BEGIN EXCLUSIVE",
        };
        state
            .db
            .execute_batch(query)
            .map(move |_| ProcessTransaction {
                state,
                drop_behavior: DropBehavior::Rollback,
            })
    }

    #[inline]
    pub(crate) fn set_drop_behavior(&mut self, drop_behavior: DropBehavior) {
        self.drop_behavior = drop_behavior;
    }

    #[inline]
    pub(crate) fn finish(mut self) -> rusqlite::Result<()> {
        self.finish_()
    }

    #[inline]
    pub(crate) fn commit(mut self) -> rusqlite::Result<()> {
        self.drop_behavior = DropBehavior::Commit;
        self.finish()
    }

    #[inline]
    pub(crate) fn state(&self) -> &ProcessState {
        self.state
    }

    fn finish_(&mut self) -> rusqlite::Result<()> {
        match self.drop_behavior {
            DropBehavior::Ignore => Ok(()),
            DropBehavior::Commit => match self.state.db.execute_batch("COMMIT") {
                Ok(()) => {
                    self.state.wrote = 0;
                    Ok(())
                }
                Err(e) => {
                    let _ = self.state.db.execute_batch("ROLLBACK");
                    self.state.wrote = 0;
                    Err(e)
                }
            },
            DropBehavior::Rollback => {
                self.state.wrote = 0;
                self.state.db.execute_batch("ROLLBACK")
            }
            DropBehavior::Panic => panic!("ProcessTransaction dropped"),
        }
    }
}

impl<'a> Drop for ProcessTransaction<'a> {
    #[inline]
    fn drop(&mut self) {
        let _ = self.finish_();
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

/// An object representing a source or target in the redo database.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub(crate) struct File {
    pub(crate) id: i64,
    pub(crate) name: String,
    pub(crate) is_generated: bool,
    pub(crate) is_override: bool,
    pub(crate) checked_runid: Option<i64>,
    pub(crate) changed_runid: Option<i64>,
    pub(crate) failed_runid: Option<i64>,
    pub(crate) stamp: Option<String>,
    pub(crate) csum: Option<String>,
}

const FILE_QUERY_PREFIX: &str = "select rowid, \
                              name, \
                              is_generated, \
                              is_override, \
                              checked_runid, \
                              changed_runid, \
                              failed_runid, \
                              stamp, \
                              csum \
                              from Files ";

impl File {
    pub(crate) fn from_name(
        ptx: &mut ProcessTransaction,
        name: &str,
        allow_add: bool,
    ) -> Result<File, Error> {
        let mut q = String::from(FILE_QUERY_PREFIX);
        q.push_str("where name=?");
        let normalized_name: Cow<str> = if name == ALWAYS {
            Cow::Borrowed(&ALWAYS)
        } else {
            Cow::Owned(
                relpath(name, &ptx.state.env.base)?
                    .to_string_lossy()
                    .into_owned(),
            )
        };
        match ptx
            .state
            .db
            .query_row(q.as_str(), params!(&normalized_name), |row| {
                File::from_cols(&ptx.state.env, row)
            }) {
            Ok(f) => Ok(f),
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                if !allow_add {
                    return Err(format_err!("no file with name={:?}", normalized_name));
                }
                match ptx.state.write(
                    "insert into Files (name) values (?)",
                    params!(normalized_name),
                ) {
                    Ok(_) => {}
                    Err(rusqlite::Error::SqliteFailure(
                        libsqlite3_sys::Error {
                            code: libsqlite3_sys::ErrorCode::ConstraintViolation,
                            ..
                        },
                        _,
                    )) => {
                        // Some parallel redo probably added it at the same time; no
                        // big deal.
                    }
                    Err(e) => {
                        return Err(e.into());
                    }
                }
                Ok(ptx
                    .state
                    .db
                    .query_row(q.as_str(), params!(&normalized_name), |row| {
                        File::from_cols(&ptx.state.env, row)
                    })?)
            }
            Err(e) => Err(e.into()),
        }
    }

    pub(crate) fn from_id(ptx: &mut ProcessTransaction, id: i64) -> Result<File, Error> {
        let mut q = String::from(FILE_QUERY_PREFIX);
        q.push_str("where id=?");
        Ok(ptx.state.db.query_row(q.as_str(), params!(id), |row| {
            File::from_cols(&ptx.state.env, row)
        })?)
    }

    pub(crate) fn from_cols(env: &Env, row: &Row) -> rusqlite::Result<File> {
        let mut f = File {
            id: row.get("rowid")?,
            name: row.get("name")?,
            is_generated: row
                .get::<&str, Option<bool>>("is_generated")?
                .unwrap_or(false),
            is_override: row
                .get::<&str, Option<bool>>("is_override")?
                .unwrap_or(false),
            checked_runid: row.get("checked_runid")?,
            changed_runid: row.get("changed_runid")?,
            failed_runid: row.get("failed_runid")?,
            stamp: row.get("stamp")?,
            csum: row.get("csum")?,
        };
        if f.name == ALWAYS {
            if let Some(env_runid) = env.runid {
                f.changed_runid = Some(
                    f.changed_runid
                        .map(|changed_runid| cmp::max(env_runid, changed_runid))
                        .unwrap_or(env_runid),
                );
            }
        }
        Ok(f)
    }

    pub(crate) fn refresh(&mut self, ptx: &mut ProcessTransaction) -> Result<(), Error> {
        *self = File::from_id(ptx, self.id)?;
        Ok(())
    }

    fn set_checked(&mut self, v: &Env) {
        self.checked_runid = v.runid;
    }

    pub(crate) fn nice_name(&self, v: &Env) -> Result<String, Error> {
        Ok(relpath(v.base.join(&self.name), &v.startdir)?
            .to_string_lossy()
            .into_owned())
    }
}

#[derive(Debug)]
pub(crate) struct LockManager {
    file: fs::File,
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

    /// Detect Windows WSL's completely broken `fcntl()` locks.
    ///
    /// Symptom: locking a file always returns success, even if other processes
    /// also think they have it locked. See
    /// https://github.com/Microsoft/WSL/issues/1927 for more details.
    ///
    /// Bug exists at least in WSL "4.4.0-17134-Microsoft #471-Microsoft".
    ///
    /// Returns `true` if broken, `false` otherwise.
    pub(crate) fn detect_broken_locks(manager: &Rc<LockManager>) -> nix::Result<bool> {
        let mut pl = Lock::new(manager.clone(), 0);
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
                let mut cl = Lock::new(manager.clone(), 0);
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
pub(crate) struct Lock {
    manager: Rc<LockManager>,
    owned: bool,
    fid: i32,
}

impl Lock {
    pub(crate) fn new(manager: Rc<LockManager>, fid: i32) -> Lock {
        {
            let mut locks = manager.locks.borrow_mut();
            assert!(locks.insert(fid));
        }
        Lock {
            manager,
            owned: false,
            fid,
        }
    }

    /// Check that this lock is in a sane state.
    pub(crate) fn check(&self) {
        assert!(!self.owned);
    }

    #[inline]
    pub(crate) fn is_owned(&self) -> bool {
        self.owned
    }

    #[inline]
    pub(crate) fn force_owned(&mut self) {
        self.owned = true;
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

impl Drop for Lock {
    fn drop(&mut self) {
        {
            let mut locks = self.manager.locks.borrow_mut();
            locks.remove(&self.fid);
        }
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

/// Given a relative or absolute path `t`, express it relative to `base`.
fn relpath<P1: AsRef<Path>, P2: AsRef<Path>>(t: P1, base: P2) -> io::Result<PathBuf> {
    let t = t.as_ref();
    // TODO(maybe): Memoize cwd
    let cwd = std::env::current_dir()?;
    let t = cwd.join(t);
    let t = realdirpath(&t)?;
    let t = helpers::normpath(&t);

    let base = base.as_ref();
    let base = realdirpath(&base)?;
    let base = helpers::normpath(&base);

    let mut n = 0usize;
    for (tp, bp) in t.components().zip(base.components()) {
        if tp != bp {
            break;
        }
        n += 1;
    }
    let mut buf = PathBuf::new();
    for _ in base.components().skip(n) {
        buf.push("..");
    }
    for part in t.components().skip(n) {
        buf.push(part);
    }
    Ok(buf)
}

/**
 * Like `Path::canonicalize()`, but don't follow symlinks for the last element.
 *
 * redo needs this because targets can be symlinks themselves, and we want
 * to talk about the symlink, not what it points at.  However, all the path
 * elements along the way could result in pathname aliases for a *particular*
 * target, so we want to resolve it to one unique name.
 */
fn realdirpath<'a, P>(t: &'a P) -> io::Result<Cow<'a, Path>>
where
    P: AsRef<Path> + ?Sized,
{
    let t = t.as_ref();
    match t.parent() {
        None => Ok(Cow::Borrowed(t)),
        Some(dname) => match t.file_name() {
            None => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid file name",
            )),
            Some(fname) => {
                let mut buf = dname.canonicalize()?;
                buf.push(fname);
                Ok(Cow::Owned(buf))
            }
        },
    }
}
