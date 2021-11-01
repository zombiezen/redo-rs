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

use std::env;
use std::path::Path;
use std::process::Command;

#[test]
fn integration_test() {
    let crate_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let redo_path = Path::new(env!("CARGO_BIN_EXE_redo"));

    let status = clear_redo_env(&mut Command::new(redo_path))
        .current_dir(crate_dir)
        .arg(Path::new("redo").join("py"))
        .arg(Path::new("redo").join("sh"))
        .arg(Path::new("redo").join("whichpython"))
        .env("RUST_BACKTRACE", "1")
        .spawn()
        .expect("could not build prereqs")
        .wait()
        .expect("could not get exit status");
    assert!(status.success(), "prereq build status = {:?}", status);

    let status = clear_redo_env(&mut Command::new(redo_path))
        .current_dir(crate_dir.join("t"))
        .env("RUST_BACKTRACE", "1")
        .spawn()
        .expect("could not start integration test")
        .wait()
        .expect("could not get exit status");
    assert!(status.success(), "integration test status = {:?}", status);
}

fn clear_redo_env(cmd: &mut Command) -> &mut Command {
    for (k, _) in env::vars_os() {
        if k.to_str()
            .map(|k| k.starts_with("REDO_") || k == "REDO" || k == "MAKEFLAGS" || k == "DO_BUILT")
            .unwrap_or(false)
        {
            cmd.env_remove(k);
        }
    }
    cmd
}
