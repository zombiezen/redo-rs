use failure::{format_err, Error, ResultExt};
use libc::{self, c_int, c_short, flock, off_t};
use libsqlite3_sys;
use nix;
use nix::errno::Errno;
use nix::fcntl::{self, FcntlArg};
use nix::sys::wait::{self, WaitStatus};
use nix::unistd::{self, ForkResult};
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSqlOutput, ValueRef};
use rusqlite::{
    self, params, Connection, DropBehavior, OptionalExtension, Row, ToSql, TransactionBehavior,
    NO_PARAMS,
};
use std::borrow::Cow;
use std::cell::RefCell;
use std::cmp;
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::{self, Metadata, OpenOptions};
use std::io;
use std::mem;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process;
use std::rc::Rc;
use std::str;
use std::time::{Duration, SystemTime};

use super::env::Env;
use super::helpers;

const SCHEMA_VER: i32 = 2;

/// An invalid filename that is always marked as dirty.
pub const ALWAYS: &str = "//ALWAYS";

/// fid offset for "log locks".
pub(crate) const LOG_LOCK_MAGIC: i32 = 0x10000000;

/// Connection to the state database.
#[derive(Debug)]
#[non_exhaustive]
pub struct ProcessState {
    db: Connection,
    lock_manager: Rc<LockManager>,
    env: Env,
    wrote: i32,
}

#[derive(Debug)]
pub struct ProcessTransaction<'a> {
    state: Option<&'a mut ProcessState>,
    drop_behavior: DropBehavior,
}

impl ProcessState {
    pub fn init(mut e: Env) -> Result<ProcessState, Error> {
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
        if e.is_toplevel() && LockManager::detect_broken_locks(&lock_manager)? {
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
            wrote: 0,
        })
    }

    #[inline]
    pub fn env(&self) -> &Env {
        &self.env
    }

    #[inline]
    pub(crate) fn new_lock(&self, fid: i32) -> Lock {
        Lock::new(self.lock_manager.clone(), fid)
    }

    #[inline]
    pub fn is_toplevel(&self) -> bool {
        self.env.is_toplevel()
    }

    #[inline]
    pub fn is_flushed(&self) -> bool {
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
    pub fn new(
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
                state: Some(state),
                drop_behavior: DropBehavior::Rollback,
            })
    }

    #[inline]
    pub fn set_drop_behavior(&mut self, drop_behavior: DropBehavior) {
        self.drop_behavior = drop_behavior;
    }

    #[inline]
    pub(crate) fn finish(mut self) -> rusqlite::Result<&'a mut ProcessState> {
        self.finish_()
    }

    #[inline]
    pub fn commit(mut self) -> rusqlite::Result<&'a mut ProcessState> {
        self.drop_behavior = DropBehavior::Commit;
        self.finish()
    }

    #[inline]
    pub fn state(&self) -> &ProcessState {
        *self.state.as_ref().unwrap()
    }

    fn write<P>(&mut self, sql: &str, params: P) -> rusqlite::Result<usize>
    where
        P: IntoIterator,
        P::Item: ToSql,
    {
        let state = self.state.take().unwrap();
        let result = state.write(sql, params);
        self.state = Some(state);
        result
    }

    fn finish_(&mut self) -> rusqlite::Result<&'a mut ProcessState> {
        let state = self.state.take().unwrap();
        match self.drop_behavior {
            DropBehavior::Ignore => Ok(state),
            DropBehavior::Commit => match state.db.execute_batch("COMMIT") {
                Ok(()) => {
                    state.wrote = 0;
                    Ok(state)
                }
                Err(e) => {
                    let _ = state.db.execute_batch("ROLLBACK");
                    state.wrote = 0;
                    Err(e)
                }
            },
            DropBehavior::Rollback => {
                state.wrote = 0;
                state.db.execute_batch("ROLLBACK")?;
                Ok(state)
            }
            DropBehavior::Panic => panic!("ProcessTransaction dropped"),
        }
    }
}

impl<'a> Drop for ProcessTransaction<'a> {
    #[inline]
    fn drop(&mut self) {
        if self.state.is_some() {
            let _ = self.finish_();
        }
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
pub struct File {
    pub(crate) id: i64,
    pub(crate) name: String,
    pub(crate) is_generated: bool,
    pub(crate) is_override: bool,
    pub(crate) checked_runid: Option<i64>,
    pub(crate) changed_runid: Option<i64>,
    pub(crate) failed_runid: Option<i64>,
    pub(crate) stamp: Option<Stamp>,
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
    pub fn from_name(
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
                relpath(name, &ptx.state().env.base)?
                    .to_string_lossy()
                    .into_owned(),
            )
        };
        match ptx
            .state()
            .db
            .query_row(q.as_str(), params!(&normalized_name), |row| {
                File::from_cols(&ptx.state().env, row)
            }) {
            Ok(f) => Ok(f),
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                if !allow_add {
                    return Err(format_err!("no file with name={:?}", normalized_name));
                }
                match ptx.write(
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
                    .state()
                    .db
                    .query_row(q.as_str(), params!(&normalized_name), |row| {
                        File::from_cols(&ptx.state().env, row)
                    })?)
            }
            Err(e) => Err(e.into()),
        }
    }

    pub(crate) fn from_id(ptx: &mut ProcessTransaction, id: i64) -> Result<File, Error> {
        let mut q = String::from(FILE_QUERY_PREFIX);
        q.push_str("where rowid=?");
        Ok(ptx.state().db.query_row(q.as_str(), params!(id), |row| {
            File::from_cols(&ptx.state().env, row)
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

    #[inline]
    pub fn is_generated(&self) -> bool {
        self.is_generated
    }

    pub(crate) fn refresh(&mut self, ptx: &mut ProcessTransaction) -> Result<(), Error> {
        *self = File::from_id(ptx, self.id)?;
        Ok(())
    }

    /// Write the file to the database.
    pub fn save(&mut self, ptx: &mut ProcessTransaction) -> Result<(), Error> {
        ptx.write(
            "update Files set is_generated=?, \
                              is_override=?, \
                              checked_runid=?, \
                              changed_runid=?, \
                              failed_runid=?, \
                              stamp=?, \
                              csum=? where rowid=?",
            params!(
                self.is_generated,
                self.is_override,
                self.checked_runid,
                self.changed_runid,
                self.failed_runid,
                self.stamp,
                self.csum,
                self.id
            ),
        )?;
        Ok(())
    }

    fn set_checked(&mut self, v: &Env) {
        self.checked_runid = v.runid;
    }

    pub fn set_changed(&mut self, v: &Env) {
        self.changed_runid = v.runid;
        self.failed_runid = None;
        self.is_override = false;
    }

    pub(crate) fn set_failed(&mut self, v: &Env) -> Result<(), Error> {
        self.update_stamp(v, false)?;
        self.failed_runid = v.runid;

        // if we failed and the target file still exists,
        // then we're generated.
        //
        // if the target file now does *not* exist, then go back to
        // treating this as a source file.  Since it doesn't exist,
        // if someone tries to rebuild it immediately, it'll go
        // back to being a target.  But if the file is manually
        // created before that, we don't need a "manual override"
        // warning.
        self.is_generated = !self.stamp.as_ref().map(|s| s.is_missing()).unwrap_or(false);
        Ok(())
    }

    pub(crate) fn set_static(&mut self, v: &Env) -> Result<(), Error> {
        self.update_stamp(v, true)?;
        self.failed_runid = None;
        self.is_override = false;
        self.is_generated = false;
        Ok(())
    }

    fn set_override(&mut self, v: &Env) -> Result<(), Error> {
        self.update_stamp(v, false)?;
        self.failed_runid = None;
        self.is_override = true;
        Ok(())
    }

    /// Sets the file's stamp.
    pub fn set_stamp(&mut self, newstamp: Stamp) {
        self.stamp = Some(newstamp);
    }

    pub(crate) fn update_stamp(&mut self, v: &Env, must_exist: bool) -> Result<(), Error> {
        let newstamp = self.read_stamp(v)?;
        if must_exist && newstamp.is_missing() {
            return Err(format_err!("{:?} does not exist", self.name));
        }
        if self.stamp.as_ref() != Some(&newstamp) {
            self.stamp = Some(newstamp);
            self.set_changed(v);
        }
        Ok(())
    }

    /// Reports if this object represents a source (not a target).
    pub(crate) fn is_source(&self, v: &Env) -> Result<bool, Error> {
        if self.name.starts_with("//") {
            // Special name, ignore.
            return Ok(false);
        }
        let newstamp = self.read_stamp(v)?;
        if self.is_generated
            && (!self.is_failed(v) || !newstamp.is_missing())
            && !self.is_override
            && self.stamp.as_ref() == Some(&newstamp)
        {
            // Target is as we left it.
            return Ok(false);
        }
        if (!self.is_generated || self.stamp.as_ref() != Some(&newstamp)) && newstamp.is_missing() {
            // Target has gone missing after the last build.
            // It's not usefully a source *or* a target.
            return Ok(false);
        }
        Ok(true)
    }

    /// Reports if this object represents a target (not a source).
    pub(crate) fn is_target(&self, v: &Env) -> Result<bool, Error> {
        if !self.is_generated {
            return Ok(false);
        }
        self.is_source(v).map(|b| !b)
    }

    pub(crate) fn is_checked(&self, v: &Env) -> bool {
        match self.checked_runid {
            Some(checked_runid) => checked_runid != 0 && checked_runid >= v.runid.unwrap(),
            None => false,
        }
    }

    pub(crate) fn is_changed(&self, v: &Env) -> bool {
        match self.changed_runid {
            Some(changed_runid) => changed_runid != 0 && changed_runid >= v.runid.unwrap(),
            None => false,
        }
    }

    pub(crate) fn is_failed(&self, v: &Env) -> bool {
        match self.failed_runid {
            Some(failed_runid) => failed_runid != 0 && failed_runid >= v.runid.unwrap(),
            None => false,
        }
    }

    /// Mark the list of dependencies of this object as deprecated.
    ///
    /// We do this when starting a new build of the current target.  We don't
    /// delete them right away, because if the build fails, we still want to
    /// know the old deps.
    pub(crate) fn zap_deps1(&mut self, ptx: &mut ProcessTransaction) -> Result<(), Error> {
        ptx.write(
            "update Deps set delete_me=? where target=?",
            params!(true, self.id),
        )?;
        Ok(())
    }

    /// Delete any deps that were *not* referenced in the current run.
    ///
    /// Dependencies of a given target can change from one build to the next.
    /// We forget old dependencies only after a build completes successfully.
    pub(crate) fn zap_deps2(&mut self, ptx: &mut ProcessTransaction) -> Result<(), Error> {
        ptx.write(
            "delete from Deps where target=? and delete_me=1",
            params!(self.id),
        )?;
        Ok(())
    }

    /// Add a dependency and write it to the database.
    pub fn add_dep(
        &mut self,
        ptx: &mut ProcessTransaction,
        mode: DepMode,
        dep: &str,
    ) -> Result<(), Error> {
        let src = File::from_name(ptx, dep, true)?;
        assert_ne!(self.id, src.id);
        ptx.write(
            "insert or replace into Deps (target, mode, source, delete_me) values (?,?,?,?)",
            params!(self.id, mode, src.id, false),
        )?;
        Ok(())
    }

    fn read_stamp_st<F: FnOnce(&Path) -> io::Result<Metadata>>(
        &self,
        v: &Env,
        statfunc: F,
    ) -> Result<(bool, Stamp), Error> {
        match statfunc(&v.base.join(&self.name)) {
            Ok(metadata) => Ok((
                metadata.file_type().is_symlink(),
                Stamp::from_metadata(&metadata)?,
            )),
            Err(e) => {
                if e.kind() == io::ErrorKind::NotFound {
                    Ok((false, Stamp::MISSING))
                } else {
                    Err(e.into())
                }
            }
        }
    }

    pub(crate) fn read_stamp(&self, v: &Env) -> Result<Stamp, Error> {
        let (is_link, pre) = self.read_stamp_st(v, |p| fs::symlink_metadata(p))?;
        Ok(if is_link {
            // if we're a symlink, we actually care about the link object
            // itself, *and* the target of the link.  If either changes,
            // we're considered dirty.
            //
            // On the other hand, detect_override() doesn't care about the
            // target of the link, only the link itself.
            let (_, post) = self.read_stamp_st(v, |p| fs::metadata(p))?;
            pre.with_link_target(&post)
        } else {
            pre
        })
    }

    pub fn nice_name(&self, v: &Env) -> Result<String, Error> {
        Ok(relpath(v.base.join(&self.name), &v.startdir)?
            .to_string_lossy()
            .into_owned())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
#[non_exhaustive]
#[repr(u8)]
pub enum DepMode {
    Created = b'c',
    Modified = b'm',
}

impl DepMode {
    fn from_char(c: u8) -> Option<DepMode> {
        if c == DepMode::Created as u8 {
            Some(DepMode::Created)
        } else if c == DepMode::Modified as u8 {
            Some(DepMode::Modified)
        } else {
            None
        }
    }

    fn into_str(self) -> &'static str {
        const CREATED_STR: &[u8] = &[DepMode::Created as u8];
        const MODIFIED_STR: &[u8] = &[DepMode::Modified as u8];
        unsafe {
            str::from_utf8_unchecked(match self {
                DepMode::Created => CREATED_STR,
                DepMode::Modified => MODIFIED_STR,
            })
        }
    }
}

impl From<DepMode> for u8 {
    #[inline]
    fn from(mode: DepMode) -> u8 {
        mode as u8
    }
}

impl From<DepMode> for &'static str {
    #[inline]
    fn from(mode: DepMode) -> &'static str {
        mode.into_str()
    }
}

impl FromSql for DepMode {
    fn column_result(value: ValueRef) -> FromSqlResult<DepMode> {
        match value {
            ValueRef::Text(&[c]) => DepMode::from_char(c)
                .ok_or_else(|| FromSqlError::Other(format_err!("unknown dep mode {:?}", c).into())),
            ValueRef::Text(s) => Err(FromSqlError::Other(
                format_err!("unknown dep mode {:?}", s).into(),
            )),
            _ => Err(FromSqlError::InvalidType),
        }
    }
}

impl ToSql for DepMode {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput> {
        Ok(ToSqlOutput::Borrowed(ValueRef::Text(
            self.into_str().as_bytes(),
        )))
    }
}

/// Serialized file metadata.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct Stamp(Cow<'static, str>);

impl Stamp {
    /// The stamp of a directory; mtime is unhelpful.
    pub const DIR: Stamp = Stamp(Cow::Borrowed("dir"));

    /// The stamp of a nonexistent file.
    pub const MISSING: Stamp = Stamp(Cow::Borrowed("0"));

    fn from_metadata(metadata: &fs::Metadata) -> Result<Stamp, Error> {
        use std::os::unix::fs::MetadataExt;

        if metadata.is_dir() {
            // Directories change too much; detect only existence.
            return Ok(Stamp::DIR);
        }
        // A "unique identifier" stamp for a regular file.
        let mtime = metadata
            .modified()?
            .duration_since(SystemTime::UNIX_EPOCH)?
            .as_secs_f64();
        Ok(Stamp(Cow::Owned(format!(
            "{:.6}-{}-{}-{}-{}-{}",
            mtime,
            metadata.len(),
            metadata.ino(),
            metadata.mode(),
            metadata.uid(),
            metadata.gid()
        ))))
    }

    fn with_link_target(self, dst_stamp: &Stamp) -> Stamp {
        let mut s = self.0.into_owned();
        s.push_str("+");
        s.push_str(&dst_stamp.0);
        Stamp(Cow::Owned(s))
    }

    #[inline]
    pub(crate) fn is_missing(&self) -> bool {
        self == &Stamp::MISSING
    }

    /// Determine if two stamps differ in a way that means manual override.
    ///
    /// When two stamps differ at all, that means the source is dirty and so we
    /// need to rebuild.  If they differ in mtime or size, then someone has surely
    /// edited the file, and we don't want to trample their changes.
    ///
    /// But if the only difference is something else (like ownership, st_mode,
    /// etc) then that might be a false positive; it's annoying to mark as
    /// overridden in that case, so we return `false`.  (It's still dirty though!)
    pub(crate) fn detect_override(stamp1: &Stamp, stamp2: &Stamp) -> bool {
        if stamp1 == stamp2 {
            return false;
        }
        let crit1 = stamp1.0.splitn(3, '-').take(2);
        let crit2 = stamp2.0.splitn(3, '-').take(2);
        !crit1.eq(crit2)
    }
}

impl Default for Stamp {
    #[inline]
    fn default() -> Stamp {
        Stamp::MISSING
    }
}

impl From<String> for Stamp {
    #[inline]
    fn from(s: String) -> Stamp {
        Stamp(Cow::Owned(s))
    }
}

impl FromSql for Stamp {
    fn column_result(value: ValueRef) -> FromSqlResult<Stamp> {
        String::column_result(value).map(|s| s.into())
    }
}

impl ToSql for Stamp {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput> {
        Ok(ToSqlOutput::Borrowed(ValueRef::Text(self.0.as_bytes())))
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
        helpers::close_on_exec(file.as_raw_fd(), true)?;
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
        match unsafe { unistd::fork() } {
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
    fn new(manager: Rc<LockManager>, fid: i32) -> Lock {
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
pub(crate) fn relpath<P1: AsRef<Path>, P2: AsRef<Path>>(t: P1, base: P2) -> io::Result<PathBuf> {
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
        Some(dname) => {
            let fname = t.file_name().unwrap_or(OsStr::new(".."));
            let mut buf = dname.canonicalize().or_else(|e| match e.kind() {
                io::ErrorKind::NotFound => Ok(helpers::normpath(dname).into_owned()),
                _ => Err(e),
            })?;
            buf.push(fname);
            Ok(Cow::Owned(buf))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! realdirpath_tests {
        ($($name:ident: $value:expr,)*) => {
            $(
                #[test]
                fn $name() {
                let (t, want) = $value;
                assert_eq!(want, realdirpath(&t).unwrap().as_os_str());
                }
            )*
        }
    }

    realdirpath_tests!(
        realdirpath_root: ("/", "/"),
        realdirpath_top: ("/foo.txt", "/foo.txt"),
        realdirpath_trailing_parent: ("/foo/..", "/foo/.."),
        realdirpath_intermediate_parent: ("/foo/../bar", "/bar"),
    );

    macro_rules! relpath_tests {
        ($($name:ident: $value:expr,)*) => {
            $(
                #[test]
                fn $name() {
                let (t, base, want) = $value;
                assert_eq!(
                    want,
                    relpath(&t, &base).unwrap().as_os_str(),
                    "t={:?}, base={:?}",
                    t,
                    base,
                );
                }
            )*
        }
    }

    relpath_tests!(
        relpath_basic: ("/a/b/c", "/a", "b/c"),
        relpath_different_root: ("/a/b/c", "/d", "../a/b/c"),
        relpath_tricky_parents: ("/home/light/src/github.com/zombiezen/redo-rs/.redo/../test.redo.tmp", "/home/light/src/github.com/zombiezen/redo-rs/.redo/..", "test.redo.tmp"),
    );
}
