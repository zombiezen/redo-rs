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

use failure::Error;
use rusqlite::TransactionBehavior;
use std::io;
use std::path::PathBuf;

use redo::logs::LogBuilder;
use redo::{self, DepMode, Env, ProcessState, ProcessTransaction, Stamp};

pub(crate) fn run() -> Result<(), Error> {
    let env = Env::inherit()?;
    LogBuilder::from(&env).setup(&env, io::stderr());

    let mut me = PathBuf::new();
    me.push(env.startdir());
    me.push(env.pwd());
    me.push(env.target());
    let mut ps = ProcessState::init(env)?;
    let mut ptx = ProcessTransaction::new(&mut ps, TransactionBehavior::Deferred)?;
    let mut f = redo::File::from_name(&mut ptx, &me, true)?;
    f.add_dep(&mut ptx, DepMode::Modified, redo::always_filename())?;
    let mut always = redo::File::from_name(&mut ptx, redo::always_filename(), true)?;
    always.set_stamp(Stamp::MISSING);
    always.set_changed(ptx.state().env());
    always.save(&mut ptx)?;
    ptx.commit()?;
    Ok(())
}
