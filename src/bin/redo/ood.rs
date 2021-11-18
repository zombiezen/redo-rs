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

//! List out-of-date targets (ood) targets.

use anyhow::{anyhow, Error};
use rusqlite::TransactionBehavior;
use std::cell::RefCell;
use std::collections::HashSet;
use std::env;
use std::io;

use redo::logs::LogBuilder;
use redo::{
    self, DirtyCallbacksBuilder, Env, File, Files, ProcessState, ProcessTransaction, RedoPath,
};

pub(crate) fn run() -> Result<(), Error> {
    if env::args_os().len() != 1 {
        return Err(anyhow!("no arguments expected."));
    }

    let targets: &[&RedoPath] = &[];
    let env = Env::init(targets)?;
    LogBuilder::from(&env).setup(&env, io::stderr());

    let mut ps = ProcessState::init(env)?;
    let env2 = ps.env().clone();
    let mut ptx = ProcessTransaction::new(&mut ps, TransactionBehavior::Deferred)?;
    let cache: RefCell<HashSet<i64>> = RefCell::new(HashSet::new());
    let mut cb = DirtyCallbacksBuilder::new()
        .is_checked(|f, _| cache.borrow().contains(&f.id()))
        .set_checked(|f, _| {
            cache.borrow_mut().insert(f.id());
            Ok(())
        })
        .log_override(|_| {})
        .build();
    let mut targets: Vec<File> = Vec::new();
    for resf in Files::list(&mut ptx) {
        let f = resf?;
        if f.is_target(&env2)? {
            targets.push(f);
        }
    }
    let cwd = env::current_dir()?;
    for mut f in targets {
        if !redo::is_dirty(&mut ptx, &mut f, &mut cb)?.is_clean() {
            let p = redo::relpath(env2.base().join(f.name()), &cwd)?;
            println!(
                "{}",
                p.as_os_str()
                    .to_str()
                    .ok_or(anyhow!("could not get filename as UTF-8"))?
            );
        }
    }
    Ok(())
}
