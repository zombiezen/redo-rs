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

use libc::{self, c_short, flock, off_t};
use libsqlite3_sys;
use nix;
use nix::errno::Errno;
use nix::fcntl::{self, FcntlArg};
use nix::sys::wait::{self, WaitStatus};
use nix::unistd::{self, ForkResult};
use ouroboros::self_referencing;
use rusqlite::types::{FromSql, FromSqlError, FromSqlResult, ToSqlOutput, ValueRef};
use rusqlite::{
    self, params, Connection, DropBehavior, OptionalExtension, Params, Row, Rows, Statement, ToSql,
    TransactionBehavior,
};
use std::borrow::Cow;
use std::cell::RefCell;
use std::cmp;
use std::collections::HashSet;
use std::env;
use std::ffi::OsStr;
use std::fs::{self, Metadata, OpenOptions};
use std::io;
use std::iter::FusedIterator;
use std::mem;
use std::num::TryFromIntError;
use std::os::unix::io::AsRawFd;
use std::path::{self, Path, PathBuf};
use std::process;
use std::rc::Rc;
use std::str;
use std::time::{Duration, SystemTime};

use super::cycles;
use super::env::Env;
use super::error::{RedoError, RedoErrorKind};
use super::exits::*;
use super::helpers::{self, OsBytes, RedoPath, RedoPathBuf};

const SCHEMA_VER: i32 = 2;

/// An invalid filename that is always marked as dirty.
const ALWAYS: &str = "//ALWAYS";

/// Returns an invalid filename that is always marked as dirty.
#[inline]
pub fn always_filename() -> &'static RedoPath {
    unsafe { RedoPath::from_str_unchecked(ALWAYS) }
}

/// fid offset for "log locks".
pub const LOG_LOCK_MAGIC: i64 = 0x10000000;

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
    pub fn init(mut e: Env) -> Result<ProcessState, RedoError> {
        let dbdir = {
            let mut dbdir = PathBuf::from(e.base());
            dbdir.push(".redo");
            dbdir
        };
        if let Err(err) = fs::create_dir(&dbdir) {
            if err.kind() != io::ErrorKind::AlreadyExists {
                return Err(RedoError::wrap(err, "Could not create database directory"));
            }
        }
        let lockfile = {
            let mut lockfile = PathBuf::from(&dbdir);
            lockfile.push("locks");
            lockfile
        };
        let lock_manager = LockManager::open(lockfile)?;
        if e.is_toplevel() && LockManager::detect_broken_locks(lock_manager.clone())? {
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
                db = connect(&e, &dbfile)
                    .map_err(|e| RedoError::new(format!("could not connect: {}", e)))?;
                let tx = db.transaction().map_err(RedoError::opaque_error)?;
                let ver: Option<i32> = tx
                    .query_row("select version from Schema", [], |row| row.get(0))
                    .optional()
                    .map_err(|e| RedoError::wrap(e, "schema version check failed"))?;
                if ver != Some(SCHEMA_VER) {
                    return Err(RedoError::new(format!(
                        "{}: found v{} (expected v{})\nmanually delete .redo dir to start over.",
                        dbfile.to_string_lossy(),
                        ver.unwrap_or(0),
                        SCHEMA_VER
                    )));
                }
                tx
            } else {
                helpers::unlink(&dbfile).map_err(RedoError::opaque_error)?;
                db = connect(&e, &dbfile)
                    .map_err(|e| RedoError::new(format!("could not connect: {}", e)))?;
                let tx = db.transaction().map_err(RedoError::opaque_error)?;
                tx.execute(
                    "create table Schema \
                        (version int)",
                    [],
                )
                .map_err(|e| RedoError::wrap(e, "failed to create table Schema"))?;
                tx.execute(
                    "create table Runid \
                        (id integer primary key autoincrement)",
                    [],
                )
                .map_err(|e| RedoError::wrap(e, "failed to create table Runid"))?;
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
                    [],
                )
                .map_err(|e| RedoError::wrap(e, "failed to create table Files"))?;
                tx.execute(
                    "create table Deps \
                        (target int, \
                        source int, \
                        mode not null, \
                        delete_me int, \
                        primary key (target, source))",
                    [],
                )
                .map_err(|e| RedoError::wrap(e, "failed to create table Deps"))?;
                tx.execute(
                    "insert into Schema (version) values (?)",
                    params![SCHEMA_VER],
                )
                .map_err(|e| RedoError::wrap(e, "failed to create table Schema"))?;
                // eat the '0' runid and File id.
                // Because of the cheesy way t/flush-cache is implemented, leave a
                // lot of runids available before the "first" one so that we
                // can adjust cached values to be before the first value.
                tx.execute("insert into Runid values (1000000000)", [])
                    .map_err(|e| RedoError::wrap(e, "failed to insert initial Runid"))?;
                tx.execute("insert into Files (name) values (?)", params![ALWAYS])
                    .map_err(|e| RedoError::wrap(e, "failed to insert ALWAYS file"))?;
                tx
            };

            if e.runid.is_none() {
                tx.execute(
                    "insert into Runid values \
                        ((select max(id)+1 from Runid))",
                    [],
                )
                .map_err(|e| RedoError::wrap(e, "failed to insert new Runid"))?;
                e.fill_runid(
                    tx.query_row("select last_insert_rowid()", [], |row| row.get(0))
                        .map_err(|e| RedoError::wrap(e, "failed to read runid"))?,
                );
            }

            tx.commit().map_err(RedoError::opaque_error)?;
        }

        Ok(ProcessState {
            db,
            lock_manager,
            env: e,
            wrote: 0,
        })
    }

    /// Borrow the process state's environment variables immutably.
    #[inline]
    pub fn env(&self) -> &Env {
        &self.env
    }

    /// Borrow the process state's environment variables mutably.
    #[inline]
    pub fn env_mut(&mut self) -> &mut Env {
        &mut self.env
    }

    #[inline]
    pub fn new_lock(&self, fid: i64) -> Lock {
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
        P: Params,
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
            _ => todo!(),
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
        P: Params,
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
            _ => todo!(),
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
    db.execute("pragma synchronous = off", [])?;
    // Some old/broken versions of pysqlite on MacOS work badly with journal
    // mode PERSIST.  But WAL fails on Windows WSL due to WSL's totally broken
    // locking.  On WSL, at least PERSIST works in single-threaded mode, so
    // if we're careful we can use it, more or less.
    let journal_mode = db.query_row(
        if env.locks_broken() {
            "pragma journal_mode = PERSIST"
        } else {
            "pragma journal_mode = WAL"
        },
        [],
        |row| -> rusqlite::Result<String> { row.get(0) },
    )?;
    if env.locks_broken() {
        assert_eq!(&journal_mode, "persist");
    } else {
        assert_eq!(&journal_mode, "wal");
    }
    Ok(db)
}

/// An object representing a source or target in the redo database.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct File {
    id: i64,
    name: RedoPathBuf,
    pub(crate) is_generated: bool,
    pub(crate) is_override: bool,
    pub(crate) checked_runid: Option<i64>,
    pub(crate) changed_runid: Option<i64>,
    pub(crate) failed_runid: Option<i64>,
    pub(crate) stamp: Option<Stamp>,
    csum: String,
}

const FILE_COLS: &str = "Files.rowid as \"rowid\", \
                         name as \"name\", \
                         is_generated as \"is_generated\", \
                         is_override as \"is_override\", \
                         checked_runid as \"checked_runid\", \
                         changed_runid as \"changed_runid\", \
                         failed_runid as \"failed_runid\", \
                         stamp as \"stamp\", \
                         csum as \"csum\"";

impl File {
    pub fn from_name<'a, P: AsRef<Path> + ?Sized>(
        ptx: &mut ProcessTransaction,
        name: &'a P,
        allow_add: bool,
    ) -> Result<File, RedoError> {
        let name = name.as_ref();
        let q = format!("select {} from Files where name=?", FILE_COLS);
        let normalized_name: Cow<str> = if name == OsStr::new(ALWAYS) {
            Cow::Borrowed(&ALWAYS)
        } else {
            Cow::Owned(
                relpath(name, &ptx.state().env().base())
                    .map_err(RedoError::opaque_error)?
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
                    return Err(
                        RedoError::new(format!("no file with name={:?}", normalized_name))
                            .with_kind(RedoErrorKind::FileNotFound),
                    );
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
                        return Err(RedoError::opaque_error(e));
                    }
                }
                Ok(ptx
                    .state()
                    .db
                    .query_row(q.as_str(), params!(&normalized_name), |row| {
                        File::from_cols(&ptx.state().env, row)
                    })
                    .map_err(RedoError::opaque_error)?)
            }
            Err(e) => Err(RedoError::opaque_error(e)),
        }
    }

    pub(crate) fn from_id(ptx: &mut ProcessTransaction, id: i64) -> Result<File, RedoError> {
        let q = format!("select {} from Files where rowid=?", FILE_COLS);
        match ptx.state().db.query_row(q.as_str(), params!(id), |row| {
            File::from_cols(&ptx.state().env, row)
        }) {
            Ok(f) => Ok(f),
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                Err(RedoError::new(format!("no file with id={:?}", id))
                    .with_kind(RedoErrorKind::FileNotFound))
            }
            Err(e) => Err(RedoError::opaque_error(e)),
        }
    }

    pub(crate) fn from_cols(env: &Env, row: &Row) -> rusqlite::Result<File> {
        File::from_cols_with_runid(row, env.runid)
    }

    pub(crate) fn from_cols_with_runid(row: &Row, runid: Option<i64>) -> rusqlite::Result<File> {
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
            csum: row.get::<&str, Option<String>>("csum")?.unwrap_or_default(),
        };
        if f.name.as_str() == ALWAYS {
            if let Some(env_runid) = runid {
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
    pub fn id(&self) -> i64 {
        self.id
    }

    #[inline]
    pub fn name(&self) -> &RedoPath {
        &self.name
    }

    #[inline]
    pub fn is_generated(&self) -> bool {
        self.is_generated
    }

    #[inline]
    pub fn set_generated(&mut self) {
        self.is_generated = true;
        self.is_override = false;
        self.failed_runid = None;
    }

    /// Returns the file's checksum. An empty string means no checksum.
    #[inline]
    pub fn checksum(&self) -> &str {
        &self.csum
    }

    #[inline]
    pub fn set_checksum(&mut self, csum: String) {
        self.csum = csum;
    }

    pub(crate) fn refresh(&mut self, ptx: &mut ProcessTransaction) -> Result<(), RedoError> {
        *self = File::from_id(ptx, self.id)?;
        Ok(())
    }

    /// Write the file to the database.
    pub fn save(&mut self, ptx: &mut ProcessTransaction) -> Result<(), RedoError> {
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
                if self.csum.is_empty() {
                    None
                } else {
                    Some(&self.csum)
                },
                self.id
            ),
        )
        .map_err(RedoError::opaque_error)?;
        Ok(())
    }

    pub fn set_checked(&mut self, v: &Env) {
        self.checked_runid = v.runid;
    }

    pub(crate) fn set_checked_save(
        &mut self,
        ptx: &mut ProcessTransaction<'_>,
    ) -> Result<(), RedoError> {
        self.set_checked(ptx.state().env());
        self.save(ptx)
    }

    pub fn set_changed(&mut self, v: &Env) {
        log_debug2!("BUILT: {:?} ({:?})\n", &self.name, &self.stamp);
        self.changed_runid = v.runid;
        self.failed_runid = None;
        self.is_override = false;
    }

    pub(crate) fn set_failed(&mut self, v: &Env) -> Result<(), RedoError> {
        log_debug2!("FAILED: {:?}\n", &self.name);
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

    pub(crate) fn set_static(&mut self, v: &Env) -> Result<(), RedoError> {
        self.update_stamp(v, true)?;
        self.failed_runid = None;
        self.is_override = false;
        self.is_generated = false;
        Ok(())
    }

    pub(crate) fn set_override(&mut self, v: &Env) -> Result<(), RedoError> {
        self.update_stamp(v, false)?;
        self.failed_runid = None;
        self.is_override = true;
        Ok(())
    }

    /// Sets the file's stamp.
    pub fn set_stamp(&mut self, newstamp: Stamp) {
        self.stamp = Some(newstamp);
    }

    pub(crate) fn update_stamp(&mut self, v: &Env, must_exist: bool) -> Result<(), RedoError> {
        let newstamp = self.read_stamp(v)?;
        if must_exist && newstamp.is_missing() {
            return Err(RedoError::new(format!("{:?} does not exist", self.name)));
        }
        if self.stamp.as_ref() != Some(&newstamp) {
            log_debug2!(
                "STAMP: {}: {:?} -> {:?}\n",
                &self.name,
                &self.stamp,
                &newstamp
            );
            self.stamp = Some(newstamp);
            self.set_changed(v);
        }
        Ok(())
    }

    /// Reports if this object represents a source (not a target).
    pub fn is_source(&self, v: &Env) -> Result<bool, RedoError> {
        if self.name.as_str().starts_with("//") {
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
    pub fn is_target(&self, v: &Env) -> Result<bool, RedoError> {
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

    /// Reports whether the file already failed during this run.
    pub fn is_failed(&self, v: &Env) -> bool {
        match (self.failed_runid, v.runid) {
            (Some(failed_runid), Some(vrunid)) => failed_runid != 0 && failed_runid >= vrunid,
            _ => false,
        }
    }

    /// Return the list of objects that this object depends on.
    pub(crate) fn deps(&self, ptx: &ProcessTransaction) -> Result<Vec<(DepMode, File)>, RedoError> {
        if self.is_override || !self.is_generated {
            return Ok(Vec::new());
        }
        let mut stmt = ptx
            .state()
            .db
            .prepare(&format!(
                "select Deps.mode, Deps.source, {} \
            from Files \
            join Deps on Files.rowid = Deps.source \
            where target=?",
                FILE_COLS
            ))
            .map_err(RedoError::opaque_error)?;
        let mut rows = stmt
            .query(params!(self.id))
            .map_err(RedoError::opaque_error)?;

        let mut deps = Vec::new();
        while let Some(row) = rows.next().map_err(RedoError::opaque_error)? {
            let mode: DepMode = row.get(0).map_err(RedoError::opaque_error)?;
            let f = File::from_cols(ptx.state().env(), row).map_err(RedoError::opaque_error)?;
            deps.push((mode, f));
        }
        Ok(deps)
    }

    /// Mark the list of dependencies of this object as deprecated.
    ///
    /// We do this when starting a new build of the current target.  We don't
    /// delete them right away, because if the build fails, we still want to
    /// know the old deps.
    pub(crate) fn zap_deps1(&mut self, ptx: &mut ProcessTransaction) -> Result<(), RedoError> {
        log_debug2!("zap-deps1: {:?}\n", &self.name);
        ptx.write(
            "update Deps set delete_me=? where target=?",
            params!(true, self.id),
        )
        .map_err(RedoError::opaque_error)?;
        Ok(())
    }

    /// Delete any deps that were *not* referenced in the current run.
    ///
    /// Dependencies of a given target can change from one build to the next.
    /// We forget old dependencies only after a build completes successfully.
    pub(crate) fn zap_deps2(&mut self, ptx: &mut ProcessTransaction) -> Result<(), RedoError> {
        log_debug2!("zap-deps2: {:?}\n", &self.name);
        ptx.write(
            "delete from Deps where target=? and delete_me=1",
            params!(self.id),
        )
        .map_err(RedoError::opaque_error)?;
        Ok(())
    }

    /// Add a dependency and write it to the database.
    pub fn add_dep<'a, P: AsRef<Path> + ?Sized>(
        &mut self,
        ptx: &mut ProcessTransaction,
        mode: DepMode,
        dep: &'a P,
    ) -> Result<(), RedoError> {
        let src = File::from_name(ptx, dep, true)?;
        log_debug3!(
            "add-dep: \"{}\" < {:?} \"{}\"\n",
            &self.name,
            mode,
            &src.name
        );
        assert_ne!(
            self.id,
            src.id,
            "{} cannot depend on itself",
            dep.as_ref().display()
        );
        ptx.write(
            "insert or replace into Deps (target, mode, source, delete_me) values (?,?,?,?)",
            params!(self.id, mode, src.id, false),
        )
        .map_err(RedoError::opaque_error)?;
        Ok(())
    }

    fn read_stamp_st<F>(&self, v: &Env, statfunc: F) -> Result<(bool, Stamp), RedoError>
    where
        F: FnOnce(&Path) -> io::Result<Metadata>,
    {
        match statfunc(&v.base().join(&self.name)) {
            Ok(metadata) => Ok((
                metadata.file_type().is_symlink(),
                Stamp::from_metadata(&metadata)?,
            )),
            Err(e) => {
                if e.kind() == io::ErrorKind::NotFound {
                    Ok((false, Stamp::MISSING))
                } else {
                    Err(RedoError::opaque_error(e))
                }
            }
        }
    }

    pub(crate) fn read_stamp(&self, v: &Env) -> Result<Stamp, RedoError> {
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

    pub fn nice_name(&self, v: &Env) -> Result<String, RedoError> {
        Ok(relpath(v.base().join(&self.name), &v.startdir)
            .map_err(RedoError::opaque_error)?
            .to_string_lossy()
            .into_owned())
    }
}

/// Iterator over files.
pub struct Files<'tx> {
    state: FilesState<'tx>,
    runid: Option<i64>,
}

impl Files<'_> {
    /// List all of the files known to redo, ordered by name.
    pub fn list<'tx>(ptx: &'tx mut ProcessTransaction) -> Files<'tx> {
        let runid = ptx.state().env().runid;
        let stmt = match ptx
            .state()
            .db
            .prepare(&format!("select {} from Files order by name", FILE_COLS))
        {
            Ok(stmt) => stmt,
            Err(e) => {
                return Files {
                    state: FilesState::Error(RedoError::opaque_error(e)),
                    runid,
                }
            }
        };
        let state_result = FilesRowsTryBuilder {
            stmt,
            rows_builder: |stmt| stmt.query([]),
        }
        .try_build();
        match state_result {
            Ok(rows) => Files {
                state: FilesState::Rows(rows),
                runid,
            },
            Err(e) => Files {
                state: FilesState::Error(RedoError::opaque_error(e)),
                runid,
            },
        }
    }
}

impl Iterator for Files<'_> {
    type Item = Result<File, RedoError>;

    fn next(&mut self) -> Option<Result<File, RedoError>> {
        let mut state = FilesState::Done;
        mem::swap(&mut state, &mut self.state);
        match state {
            FilesState::Rows(mut rows) => {
                let res = rows.with_rows_mut(|rows| -> Result<Option<File>, RedoError> {
                    match rows.next().map_err(RedoError::opaque_error)? {
                        Some(row) => {
                            let f = File::from_cols_with_runid(row, self.runid)
                                .map_err(RedoError::opaque_error)?;
                            Ok(Some(f))
                        }
                        None => Ok(None),
                    }
                });
                match res {
                    Ok(Some(f)) => {
                        self.state = FilesState::Rows(rows);
                        Some(Ok(f))
                    }
                    Ok(None) => None,
                    Err(e) => Some(Err(e)),
                }
            }
            FilesState::Error(e) => Some(Err(e)),
            FilesState::Done => None,
        }
    }
}

impl FusedIterator for Files<'_> {}

enum FilesState<'tx> {
    Rows(FilesRows<'tx>),
    Error(RedoError),
    Done,
}

#[self_referencing]
struct FilesRows<'tx> {
    stmt: Statement<'tx>,
    #[borrows(mut stmt)]
    #[covariant]
    rows: Rows<'this>,
}

/// Given the ID of a `File`, return the filename of its build log.
pub fn logname(v: &Env, fid: i64) -> PathBuf {
    let mut p = PathBuf::from(v.base());
    p.push(".redo");
    p.push(format!("log.{}", fid));
    p
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
            ValueRef::Text(&[c]) => DepMode::from_char(c).ok_or_else(|| {
                FromSqlError::Other(Box::new(RedoError::new(format!(
                    "unknown dep mode {:?}",
                    c
                ))))
            }),
            ValueRef::Text(s) => Err(FromSqlError::Other(Box::new(RedoError::new(format!(
                "unknown dep mode {:?}",
                s
            ))))),
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

    fn from_metadata(metadata: &fs::Metadata) -> Result<Stamp, RedoError> {
        use std::os::unix::fs::MetadataExt;

        if metadata.is_dir() {
            // Directories change too much; detect only existence.
            return Ok(Stamp::DIR);
        }
        // A "unique identifier" stamp for a regular file.
        let mtime = metadata
            .modified()
            .map_err(RedoError::opaque_error)?
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_err(RedoError::opaque_error)?
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
    locks: RefCell<HashSet<i64>>,
}

impl LockManager {
    pub(crate) fn open<P: AsRef<Path>>(path: P) -> Result<Rc<LockManager>, RedoError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .open(path)
            .map_err(RedoError::opaque_error)?;
        helpers::close_on_exec(file.as_raw_fd(), true).map_err(RedoError::opaque_error)?;
        Ok(Rc::new(LockManager {
            file,
            locks: RefCell::new(HashSet::new()),
        }))
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
    pub(crate) fn detect_broken_locks(manager: Rc<LockManager>) -> Result<bool, RedoError> {
        let child_manager_ref = manager.clone();
        let mut pl = Lock::new(manager, 0);
        // We wait for the lock here, just in case others are doing
        // this test at the same time.
        pl.wait_lock(LockType::Exclusive)?;
        match unsafe { unistd::fork() } {
            Ok(ForkResult::Parent { child: pid }) => match wait::waitpid(pid, None) {
                Ok(WaitStatus::Exited(_, status)) => Ok(status != EXIT_SUCCESS),
                Ok(_) => Ok(true),
                Err(e) => Err(RedoError::wrap(e, "could not check for broken locks")),
            },
            Ok(ForkResult::Child) => {
                // Doesn't actually unlock, since child process doesn't own it.
                if let Err(_) = pl.unlock() {
                    process::exit(EXIT_HELPER_FAILURE);
                }
                mem::drop(pl);
                let mut cl = Lock::new(child_manager_ref, 0);
                // parent is holding lock, which should prevent us from getting it.
                match cl.try_lock() {
                    Ok(true) => {
                        // Got the lock? Yikes, the locking system is broken!
                        process::exit(EXIT_FAILURE);
                    }
                    Ok(false) => {
                        // Failed to get the lock? Good, the parent owns it.
                        process::exit(EXIT_SUCCESS);
                    }
                    Err(_) => {
                        // Some other error occurred. Stay safe and report failure.
                        process::exit(EXIT_FAILURE);
                    }
                }
            }
            Err(e) => Err(RedoError::wrap(e, "could not check for broken locks")),
        }
    }
}

/// Types of locks.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum LockType {
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
pub struct Lock {
    manager: Rc<LockManager>,
    owned: bool,
    fid: i64,
}

impl Lock {
    fn new(manager: Rc<LockManager>, fid: i64) -> Lock {
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

    #[inline]
    pub(crate) fn file_id(&self) -> i64 {
        self.fid
    }

    /// Check that this lock is in a sane state.
    pub(crate) fn check(&self) -> Result<(), RedoError> {
        assert!(!self.owned);
        cycles::check(self.fid.to_string())?;
        Ok(())
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
    pub fn try_lock(&mut self) -> Result<bool, RedoError> {
        self.check()?;
        assert!(!self.owned);
        let result = fcntl::fcntl(
            self.manager.file.as_raw_fd(),
            FcntlArg::F_SETLK(
                &fid_flock(libc::F_WRLCK as c_short, self.fid).map_err(RedoError::opaque_error)?,
            ),
        );
        match result {
            Ok(_) => {
                self.owned = true;
                Ok(true)
            }
            Err(Errno::EACCES) | Err(Errno::EAGAIN) => Ok(false),
            Err(e) => Err(RedoError::opaque_error(e)),
        }
    }

    /// Try to acquire our lock, and wait if it's currently locked.
    pub fn wait_lock(&mut self, lock_type: LockType) -> Result<(), RedoError> {
        self.check()?;
        assert!(!self.owned);
        let fcntl_type = match lock_type {
            LockType::Exclusive => libc::F_WRLCK as c_short,
            LockType::Shared => libc::F_RDLCK as c_short,
        };
        fcntl::fcntl(
            self.manager.file.as_raw_fd(),
            FcntlArg::F_SETLKW(&fid_flock(fcntl_type, self.fid).map_err(RedoError::opaque_error)?),
        )
        .map_err(RedoError::opaque_error)?;
        self.owned = true;
        Ok(())
    }

    /// Release the lock, which we must currently own.
    pub fn unlock(&mut self) -> Result<(), RedoError> {
        assert!(self.owned, "can't unlock {} - we don't own it", self.fid);
        fcntl::fcntl(
            self.manager.file.as_raw_fd(),
            FcntlArg::F_SETLK(
                &fid_flock(libc::F_UNLCK as c_short, self.fid).map_err(RedoError::opaque_error)?,
            ),
        )
        .map_err(RedoError::opaque_error)?;
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

fn fid_flock(typ: c_short, fid: i64) -> Result<flock, TryFromIntError> {
    use std::convert::TryFrom;
    Ok(flock {
        l_type: typ,
        l_whence: libc::SEEK_SET as c_short,
        l_start: off_t::try_from(fid)?,
        l_len: 1,
        l_pid: 0,
        #[cfg(target_os = "freebsd")]
        l_sysid: 0,
    })
}

/// Given a relative or absolute path `t`, express it relative to `base`.
pub fn relpath<P1: AsRef<Path>, P2: AsRef<Path>>(t: P1, base: P2) -> io::Result<PathBuf> {
    let t = t.as_ref();
    let t = if t.is_absolute() {
        Cow::Borrowed(t)
    } else {
        // TODO(maybe): Memoize cwd
        let cwd = std::env::current_dir()?;
        Cow::Owned(cwd.join(t))
    };
    let t = realdirpath(&t)?;
    let t = helpers::normpath(&t);

    let base = base.as_ref();
    assert!(
        base.is_absolute(),
        "relpath called with relative base {:?}",
        base
    );
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

/// Return a relative path for `t` that will work after we do
/// `chdir(dirname(env.target()))`.
///
/// This is tricky!  `env.startdir()+env.pwd()` is the directory for the
/// *dofile*, when the dofile was started.  However, inside the dofile, someone
/// may have done a `chdir` to anywhere else.  `env.target()` is relative to the
/// dofile path, so we have to first figure out where the dofile was, then find
/// TARGET relative to that, then find t relative to that.
///
/// FIXME: find some cleaner terminology for all these different paths.
pub(crate) fn target_relpath<P: AsRef<Path>>(env: &Env, t: P) -> Result<RedoPathBuf, RedoError> {
    use std::convert::TryFrom;

    let cwd = std::env::current_dir().map_err(RedoError::opaque_error)?;
    let rel_dofile_dir = env.startdir().join(env.pwd());
    let dofile_dir = helpers::abs_path(&cwd, &rel_dofile_dir);
    let target_dir = if env.target().as_os_str().is_empty() {
        dofile_dir.into_owned()
    } else {
        dofile_dir
            .join(env.target())
            .parent()
            .ok_or_else(|| {
                RedoError::new(format!(
                    "invalid target path {:?} (no directory)",
                    env.target()
                ))
                .with_kind(RedoErrorKind::InvalidTarget(
                    env.target().as_os_str().to_os_string(),
                ))
            })?
            .to_path_buf()
    };
    let target_dir = helpers::abs_path(&cwd, &target_dir);
    Ok(RedoPathBuf::try_from(
        relpath(t, target_dir).map_err(RedoError::opaque_error)?,
    )?)
}

pub(crate) fn warn_override(name: &RedoPath) {
    log_warn!("{} - you modified it; skipping\n", name);
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
    let i = OsBytes::new(t)
        .collect::<Vec<u8>>()
        .iter()
        .copied()
        .rposition(|c| path::is_separator(c as char));
    let (dname, fname) = match i {
        Some(i) => {
            let dname = match OsBytes::new(t).osstr_slice(i + 1) {
                Cow::Owned(s) => Cow::Owned(PathBuf::from(s)),
                Cow::Borrowed(s) => Cow::Borrowed(Path::new(s)),
            };
            let fname: &OsStr = {
                let mut bytes = OsBytes::new(t);
                for _ in 0..(i + 1) {
                    bytes.next();
                }
                bytes.into()
            };
            (dname, Path::new(fname))
        }
        None => (Cow::Borrowed(Path::new(".")), t),
    };
    if dname == Path::new(".") {
        Ok(Cow::Borrowed(t))
    } else {
        let mut buf = match dname.canonicalize() {
            Ok(path) => path,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                let dname = if dname.is_absolute() {
                    dname
                } else {
                    Cow::Owned(env::current_dir()?.join(dname))
                };
                helpers::normpath(&dname).into_owned()
            }
            Err(e) => return Err(e),
        };
        buf.push(fname);
        Ok(Cow::Owned(buf))
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
        realdirpath_curdir: (".", "."),
        realdirpath_top: ("/foo.txt", "/foo.txt"),
        realdirpath_trailing_parent: ("/foo/..", "/foo/.."),
        realdirpath_intermediate_parent1: ("/foo/../bar", "/bar"),
        realdirpath_intermediate_parent2: ("/workspace/cmd/server/../../client/dist/../install", "/workspace/client/install"),
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
        relpath_more_tricky_parents: ("/workspace/cmd/server/../../client/dist/../install", "/workspace", "client/install"),
    );
}
