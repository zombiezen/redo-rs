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
use nix::unistd;
use rusqlite::TransactionBehavior;
use sha1::Sha1;
use std::env;
use std::io;
use std::path::PathBuf;

use redo::logs::LogBuilder;
use redo::{self, log_debug2, Env, File, ProcessState, ProcessTransaction};

fn main() {
    redo::run_program("redo-stamp", run);
}

fn run() -> Result<(), Error> {
    use sha1::Digest;

    if env::args_os().len() != 1 {
        return Err(format_err!("no arguments expected."));
    }
    if unistd::isatty(0).unwrap_or(false) {
        return Err(format_err!("you must provide the data to stamp on stdin"));
    }
    let env = Env::inherit()?;
    LogBuilder::from(&env).setup(&env, io::stderr());

    let mut sh = Sha1::new();
    io::copy(&mut io::stdin(), &mut sh)?;
    let csum = format!("{:x}", sh.finalize());

    if env.target().as_os_str().is_empty() {
        return Ok(());
    }

    let mut me = PathBuf::new();
    me.push(env.startdir());
    me.push(env.pwd());
    me.push(env.target());
    let mut ps = ProcessState::init(env)?;
    let mut ptx = ProcessTransaction::new(&mut ps, TransactionBehavior::Deferred)?;
    let mut f = File::from_name(
        &mut ptx,
        me.as_os_str()
            .to_str()
            .ok_or(format_err!("unable to convert target to string"))?,
        true,
    )?;
    let changed = csum != f.checksum();
    log_debug2!("{}: old = {}", f.name(), f.checksum());
    log_debug2!(
        "{}: sum = {} ({})",
        f.name(),
        csum,
        if changed { "changed" } else { "unchanged" }
    );
    f.set_generated();
    if changed {
        f.set_changed(ptx.state().env()); // update_stamp might skip this if mtime is identical
        f.set_checksum(csum);
    } else {
        // unchanged
        f.set_checked(ptx.state().env());
    }
    f.save(&mut ptx)?;
    ptx.commit()?;
    Ok(())
}
