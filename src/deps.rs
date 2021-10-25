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

//! Code for checking redo target dependencies.

use failure::{format_err, Error};
use std::cmp;
use std::collections::HashSet;
use std::ops::{Deref, DerefMut};

use super::state::{self, DepMode, File, ProcessTransaction, Stamp};

/// Determine if the given `File` needs to be built.
pub fn is_dirty(ptx: &mut ProcessTransaction, f: &mut File) -> Result<Dirtiness, Error> {
    let runid = ptx
        .state()
        .env()
        .runid
        .ok_or_else(|| format_err!("RUNID not set"))?;
    private_is_dirty(ptx, MutOrOwned::MutBorrowed(f), "", runid, &HashSet::new())
}

/// Determine if the given `File` needs to be built.
///
/// `depth` is a string of whitespace representing the recursion depth.
/// `max_changed` is initially the current runid:
/// if a target is newer than this,
/// anything that depends on it is considered outdated.
/// `already_checked` is the list of dependencies already checked in this recursive cycle
/// to avoid infinite loops.
fn private_is_dirty(
    ptx: &mut ProcessTransaction,
    mut f: MutOrOwned<File>,
    depth: &str,
    max_changed: i64,
    already_checked: &HashSet<i64>,
) -> Result<Dirtiness, Error> {
    if already_checked.contains(&f.id()) {
        return Err(format_err!("cyclic dependency"));
    }
    let already_checked = {
        let mut already_checked = already_checked.clone();
        already_checked.insert(f.id());
        already_checked
    };

    if ptx.state().env().debug >= 1 {
        log_debug!(
            "{}?{} {:?},{:?}\n",
            depth,
            f.nice_name(ptx.state().env())?,
            f.is_generated(),
            f.is_override
        );
    }

    if f.failed_runid.is_some() {
        log_debug!("{}-- DIRTY (failed last time)\n", depth);
        return Ok(Dirtiness::Dirty);
    }
    match f.changed_runid {
        None => {
            log_debug!("{}-- DIRTY (never built)\n", depth);
            return Ok(Dirtiness::Dirty);
        }
        Some(changed_runid) if changed_runid > max_changed => {
            log_debug!(
                "{}-- DIRTY (built {} > {}; {})\n",
                depth,
                changed_runid,
                max_changed,
                ptx.state().env().runid.unwrap_or(-1)
            );
            return Ok(Dirtiness::Dirty); // has been built more recently than parent
        }
        _ => {}
    }
    if f.is_checked(ptx.state().env()) {
        if ptx.state().env().debug >= 1 {
            log_debug!("{}-- CLEAN (checked)\n", depth);
        }
        return Ok(Dirtiness::Clean); // has already been checked during this session
    }
    match f.stamp.as_ref() {
        None => {
            log_debug!("{}-- DIRTY (no stamp)\n", depth);
            return Ok(Dirtiness::Dirty);
        }
        Some(oldstamp) => {
            let newstamp = f.read_stamp(ptx.state().env())?;
            if oldstamp != &newstamp {
                if newstamp == Stamp::MISSING {
                    log_debug!("{}-- DIRTY (missing)\n", depth);
                    if f.is_generated() {
                        // previously was stamped and generated, but suddenly missing.
                        // We can safely forget that it is/was a target; if someone
                        // does redo-ifchange on it and it doesn't exist, we'll mark
                        // it a target again, but if someone creates it by hand,
                        // it'll be a source.  This should reduce false alarms when
                        // files change from targets to sources as a project evolves.
                        log_debug!("{}  converted target -> source {:?}", depth, f.id());
                        f.is_generated = false;
                        f.failed_runid = Some(0);
                        f.save(ptx)?;
                        f.refresh(ptx)?;
                        debug_assert!(!f.is_generated());
                    }
                } else {
                    log_debug!("{}-- DIRTY (mtime)\n", depth);
                }
                return Ok(if f.csum.as_ref().map(|s| !s.is_empty()).unwrap_or(false) {
                    Dirtiness::NeedTargets(vec![f.into_owned()])
                } else {
                    Dirtiness::Dirty
                });
            }
        }
    }

    let mut must_build: Vec<File> = Vec::new();
    for (mode, f2) in f.deps(ptx)? {
        let mut dirty = Dirtiness::Clean;
        match mode {
            DepMode::Created => {
                if ptx.state().env().base().join(&f2.name).exists() {
                    log_debug!("{}-- DIRTY (created)\n", depth);
                    dirty = Dirtiness::Dirty;
                }
            }
            DepMode::Modified => {
                let sub = {
                    let mut depth = depth.to_string();
                    depth.push_str("  ");
                    private_is_dirty(
                        ptx,
                        MutOrOwned::Owned(f2),
                        &depth,
                        cmp::max(
                            f.changed_runid
                                .expect("changed_runid missing on modified file"),
                            f.checked_runid.unwrap_or(0),
                        ),
                        &already_checked,
                    )?
                };
                if !sub.is_clean() {
                    log_debug!("{}-- DIRTY (sub)\n", depth);
                    dirty = sub;
                }
            }
        }
        if f.csum.as_ref().map(|s| s.is_empty()).unwrap_or(true) {
            // f is a "normal" target: dirty f2 means f is instantly dirty
            match dirty {
                Dirtiness::Dirty => {
                    // f2 is definitely dirty, so f definitely needs to
                    // redo.
                    return Ok(Dirtiness::Dirty);
                }
                Dirtiness::NeedTargets(targets) => {
                    // our child f2 might be dirty, but it's not sure yet.  It's
                    // given us a list of targets we have to redo in order to
                    // be sure.
                    must_build.extend(targets);
                }
                _ => {}
            }
        } else {
            // f is "checksummable": dirty f2 means f needs to redo,
            // but f might turn out to be clean after that (ie. our parent
            // might not be dirty).
            match dirty {
                Dirtiness::Dirty => {
                    // f2 is definitely dirty, so f definitely needs to
                    // redo.  However, after that, f might turn out to be
                    // unchanged.
                    return Ok(Dirtiness::NeedTargets(vec![f.into_owned()]));
                }
                Dirtiness::NeedTargets(targets) => {
                    // our child f2 might be dirty, but it's not sure yet.  It's
                    // given us a list of targets we have to redo in order to
                    // be sure.
                    must_build.extend(targets);
                }
                _ => {}
            }
        }
    }
    if !must_build.is_empty() {
        // f is *maybe* dirty because at least one of its children is maybe
        // dirty.  must_build has accumulated a list of "topmost" uncertain
        // objects in the tree.  If we build all those, we can then
        // redo-ifchange f and it won't have any uncertainty next time.
        return Ok(Dirtiness::NeedTargets(must_build));
    }
    log_debug!("{}-- CLEAN\n", depth);

    // if we get here, it's because the target is clean
    if f.is_override {
        state::warn_override(&f.name);
    }
    f.set_checked_save(ptx)?;
    Ok(Dirtiness::Clean)
}

/// Result of a call to [`is_dirty`].
#[derive(Clone, Debug)]
pub enum Dirtiness {
    Clean,
    Dirty,
    NeedTargets(Vec<File>),
}

impl Dirtiness {
    /// Reports whether the dirtiness value is [`Dirtiness::Clean`].
    pub fn is_clean(&self) -> bool {
        match self {
            Dirtiness::Clean => true,
            _ => false,
        }
    }

    /// Reports whether the dirtiness value is [`Dirtiness::Dirty`].
    pub fn is_dirty(&self) -> bool {
        match self {
            Dirtiness::Dirty => true,
            _ => false,
        }
    }
}

impl Default for Dirtiness {
    fn default() -> Self {
        Dirtiness::Clean
    }
}

/// A smart pointer to owned or mutably borrowed data,
/// similar to [`std::borrow::Cow`].
#[derive(Debug)]
enum MutOrOwned<'a, B: 'a> {
    MutBorrowed(&'a mut B),
    Owned(B),
}

impl<B: Clone> MutOrOwned<'_, B> {
    /// Extracts the owned data.
    ///
    /// Clones the data if it is not already owned.
    fn into_owned(self) -> B {
        match self {
            MutOrOwned::MutBorrowed(b) => b.clone(),
            MutOrOwned::Owned(o) => o,
        }
    }
}

impl<'a, B> Deref for MutOrOwned<'a, B> {
    type Target = B;

    fn deref(&self) -> &Self::Target {
        match self {
            MutOrOwned::MutBorrowed(b) => b,
            MutOrOwned::Owned(ref o) => o,
        }
    }
}
impl<'a, B> DerefMut for MutOrOwned<'a, B> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        match self {
            MutOrOwned::MutBorrowed(b) => b,
            MutOrOwned::Owned(ref mut o) => o,
        }
    }
}
