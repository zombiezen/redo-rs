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

use anyhow::{anyhow, Error};
use rusqlite::TransactionBehavior;
use std::io;
use std::path::{Path, PathBuf};
use std::process;

use redo::logs::LogBuilder;
use redo::{self, log_err, DepMode, Env, ProcessState, ProcessTransaction, EXIT_INVALID_TARGET};

/// Build the current target if these targets are created.
pub(crate) fn run() -> Result<(), Error> {
    let env = Env::inherit()?;
    LogBuilder::from(&env).setup(io::stderr());

    let mut me = PathBuf::new();
    me.push(env.startdir());
    me.push(env.pwd());
    me.push(env.target());
    let mut ps = ProcessState::init(env)?;
    let mut ptx = ProcessTransaction::new(&mut ps, TransactionBehavior::Immediate)?;
    let mut f = redo::File::from_name(&mut ptx, &me, true)?;
    for t in std::env::args_os().skip(1) {
        if t.is_empty() {
            log_err!("cannot build the empty target (\"\").\n");
            process::exit(EXIT_INVALID_TARGET);
        }
        if Path::new(&t).exists() {
            return Err(anyhow!("{:?} already exists", t));
        }
        f.add_dep(
            &mut ptx,
            DepMode::Created,
            t.to_str()
                .ok_or_else(|| anyhow!("cannot use {:?} as target name", &t))?,
        )?;
    }
    ptx.commit()?;
    Ok(())
}
