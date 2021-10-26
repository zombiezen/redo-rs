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

//! List the known targets (not sources).

use failure::{format_err, Error};
use rusqlite::TransactionBehavior;
use std::env;
use std::io;

use redo::logs::LogBuilder;
use redo::{self, Env, Files, ProcessState, ProcessTransaction};

fn main() {
    redo::run_program("redo-targets", run);
}

fn run() -> Result<(), Error> {
    if env::args_os().len() != 1 {
        return Err(format_err!("no arguments expected."));
    }

    let targets: &[&str] = &[];
    let env = Env::init(targets)?;
    LogBuilder::from(&env).setup(&env, io::stderr());

    let cwd = env::current_dir()?;
    let mut ps = ProcessState::init(env)?;
    let env2 = ps.env().clone();
    let mut ptx = ProcessTransaction::new(&mut ps, TransactionBehavior::Deferred)?;
    for resf in Files::list(&mut ptx) {
        let f = resf?;
        if f.is_target(&env2)? {
            let p = redo::relpath(env2.base().join(f.name()), &cwd)?;
            println!(
                "{}",
                p.as_os_str()
                    .to_str()
                    .ok_or(format_err!("could not get filename as UTF-8"))?
            );
        }
    }
    Ok(())
}
