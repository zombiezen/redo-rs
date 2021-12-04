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

//! List the set of .do files considered to build a target.

use anyhow::{anyhow, Error};
use std::env;
use std::io;
use std::path::Path;
use std::process;

use redo::logs::LogBuilder;
use redo::{self, log_err, Env, EXIT_INVALID_TARGET};

pub(crate) fn run() -> Result<(), Error> {
    if env::args_os().len() != 2 {
        return Err(anyhow!("exactly one argument expected."));
    }

    let env = Env::init_no_state()?;
    LogBuilder::from(&env).setup(io::stderr());

    let want = env::args_os().nth(1).unwrap();
    if want.is_empty() {
        log_err!("cannot build the empty target (\"\").\n");
        process::exit(EXIT_INVALID_TARGET);
    }
    let cwd = env::current_dir()?;
    let want = redo::abs_path(&cwd, Path::new(&want));
    for df in redo::possible_do_files(want) {
        let do_path = df.do_dir().join(df.do_file());
        let relpath = redo::relpath(&do_path, &cwd)?;
        let relpath_str = relpath.as_os_str().to_str().unwrap();
        assert!(!relpath_str.contains('\n'));
        println!("{}", relpath_str);
        if do_path.exists() {
            return Ok(());
        }
    }

    Err(anyhow!(
        "no appropriate dofile found for {}",
        env::args().nth(1).unwrap()
    ))
}
