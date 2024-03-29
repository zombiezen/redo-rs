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

//! Internal tool for building dependencies.

use anyhow::{anyhow, Error};
use std::env;
use std::ffi::OsString;
use std::io;
use std::process::{self, Command};

use redo::logs::LogBuilder;
use redo::{self, Env, ENV_NO_OOB, ENV_UNLOCKED, EXIT_FAILURE};

pub(crate) fn run() -> Result<(), Error> {
    let mut args = env::args_os();
    if args.len() < 3 {
        return Err(anyhow!("at least 2 arguments expected."));
    }
    let env = Env::inherit()?;
    LogBuilder::from(&env).setup(io::stderr());

    let target = args.nth(1).unwrap();
    let deps: Vec<OsString> = args.collect();
    assert!(deps.iter().all(|d| d != &target));

    // Build the known dependencies of our primary target.  This *does* require
    // grabbing locks.
    let status = Command::new("redo-ifchange")
        .args(deps.iter().cloned())
        .env(ENV_NO_OOB, "1")
        .spawn()?
        .wait()?;
    if !status.success() {
        process::exit(status.code().unwrap_or(EXIT_FAILURE));
    }

    // We know our caller already owns the lock on target, so we don't have to
    // acquire another one; tell redo-ifchange about that.  Also, we keep
    // REDO_NO_OOB set, because we don't want to do OOB now either.
    // (Actually it's most important for the primary target, since it's the one
    // who initiated the OOB in the first place.)
    let status = Command::new("redo-ifchange")
        .args(deps.iter().cloned())
        .env(ENV_NO_OOB, "1")
        .env(ENV_UNLOCKED, "1")
        .spawn()?
        .wait()?;
    if !status.success() {
        process::exit(status.code().unwrap_or(EXIT_FAILURE));
    }
    Ok(())
}
