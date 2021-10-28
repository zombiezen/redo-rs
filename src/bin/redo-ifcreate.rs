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

use failure::{format_err, Error};
use rusqlite::TransactionBehavior;
use std::io;
use std::path::{Path, PathBuf};
use std::process;

use redo::logs::LogBuilder;
use redo::{self, log_err, DepMode, Env, ProcessState, ProcessTransaction};

fn main() {
    redo::run_program("redo-ifcreate", run);
}

/// Build the current target if these targets are created.
fn run() -> Result<(), Error> {
    let env = Env::inherit()?;
    LogBuilder::from(&env).setup(&env, io::stderr());

    let mut me = PathBuf::new();
    me.push(env.startdir());
    me.push(env.pwd());
    me.push(env.target());
    let mut ps = ProcessState::init(env)?;
    let mut ptx = ProcessTransaction::new(&mut ps, TransactionBehavior::Deferred)?;
    let mut f = redo::File::from_name(
        &mut ptx,
        me.as_os_str()
            .to_str()
            .expect("invalid character in target name"),
        true,
    )?;
    for t in std::env::args_os().skip(1) {
        if t.is_empty() {
            log_err!("cannot build the empty target (\"\").\n");
            process::exit(204);
        }
        if Path::new(&t).exists() {
            return Err(format_err!("{:?} already exists", t));
        }
        f.add_dep(
            &mut ptx,
            DepMode::Created,
            t.to_str()
                .ok_or_else(|| format_err!("cannot use {:?} as target name", &t))?,
        )?;
    }
    ptx.commit()?;
    Ok(())
}
