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

use clap::{App, AppSettings, Arg};
use failure::{format_err, Error};
use rusqlite::TransactionBehavior;
use std::ffi::OsString;
use std::io;
use std::path::Path;

use redo::builder::{self, StdinLogReader, StdinLogReaderBuilder};
use redo::logs::LogBuilder;
use redo::{
    self, log_err, log_warn, Dirtiness, Env, JobServer, OptionalBool, ProcessState,
    ProcessTransaction,
};

fn main() {
    redo::run_program("redo", run);
}

fn run() -> Result<(), Error> {
    let matches = App::new("redo")
        .about("Build the listed targets whether they need it or not.")
        .setting(AppSettings::DeriveDisplayOrder)
        .setting(AppSettings::UnifiedHelpMessage)
        .arg(Arg::from_usage(
            "-j, --jobs [N] 'maximum number of jobs to build at once'",
        ))
        .arg(Arg::from_usage(
            "-d, --debug... 'print dependency checks as they happen'",
        ))
        .arg(Arg::from_usage(
            "-v, --verbose... 'print commands as they are read from .do files (variables intact)'",
        ))
        .arg(Arg::from_usage(
            "-x, --xtrace... 'print commands as they are executed (variables expanded)'",
        ))
        .arg(Arg::from_usage(
            "-k, --keep-going 'keep going as long as possible even if some targets fail'",
        ))
        .arg(Arg::from_usage(
            "--shuffle 'randomize the build order to find dependency bugs'",
        ))
        .arg(
            Arg::from_usage("--no-log")
                .help("don't capture error output, just let it flow straight to stderr"),
        )
        .arg(
            Arg::from_usage("--log")
                .hidden(true)
                .overrides_with("no-log"),
        )
        .args(&redo::redo_log_flags())
        .arg(Arg::from_usage("[target]..."))
        .get_matches();
    {
        let n = matches.occurrences_of("debug");
        if n > 0 {
            std::env::set_var("REDO_DEBUG", n.to_string());
        }
    }
    {
        let n = matches.occurrences_of("verbose");
        if n > 0 {
            std::env::set_var("REDO_VERBOSE", n.to_string());
        }
    }
    {
        let n = matches.occurrences_of("xtrace");
        if n > 0 {
            std::env::set_var("REDO_XTRACE", n.to_string());
        }
    }
    if redo::auto_bool_arg(&matches, "keep-going").unwrap_or(false) {
        std::env::set_var("REDO_KEEP_GOING", "1");
    }
    if redo::auto_bool_arg(&matches, "shuffle").unwrap_or(false) {
        std::env::set_var("REDO_SHUFFLE", "1");
    }
    if redo::auto_bool_arg(&matches, "debug-locks").unwrap_or(false) {
        std::env::set_var("REDO_DEBUG_LOCKS", "1");
    }
    if redo::auto_bool_arg(&matches, "debug-pids").unwrap_or(false) {
        std::env::set_var("REDO_DEBUG_PIDS", "1");
    }
    fn set_defint(name: &str, val: OptionalBool) {
        std::env::set_var(
            name,
            std::env::var_os(name).unwrap_or_else(|| {
                OsString::from(match val {
                    OptionalBool::Off => "0",
                    OptionalBool::Auto => "1",
                    OptionalBool::On => "2",
                })
            }),
        );
    }
    set_defint("REDO_LOG", redo::auto_bool_arg(&matches, "log"));
    set_defint("REDO_PRETTY", redo::auto_bool_arg(&matches, "pretty"));
    set_defint("REDO_COLOR", redo::auto_bool_arg(&matches, "color"));
    let mut targets: Vec<&str> = matches
        .values_of("target")
        .map(|v| v.collect())
        .unwrap_or_default();

    let env = Env::init(targets.as_slice())?;
    let mut ps = ProcessState::init(env)?;
    if ps.is_toplevel() && targets.is_empty() {
        targets.push("all");
    }
    let mut j = str::parse::<i32>(matches.value_of("jobs").unwrap_or("0")).unwrap_or(0);
    if ps.is_toplevel() && (ps.env().log().unwrap_or(true) || j > 1) {
        builder::close_stdin()?;
    }
    let mut _stdin_log_reader: Option<StdinLogReader> = None;
    if ps.is_toplevel() && ps.env().log().unwrap_or(true) {
        _stdin_log_reader = Some(
            StdinLogReaderBuilder::from(ps.env())
                .set_status(redo::auto_bool_arg(&matches, "status").unwrap_or(true))
                .set_details(redo::auto_bool_arg(&matches, "details").unwrap_or(true))
                .set_debug_locks(redo::auto_bool_arg(&matches, "debug-locks").unwrap_or(false))
                .set_debug_pids(redo::auto_bool_arg(&matches, "debug-pids").unwrap_or(false))
                .start(ps.env())?,
        );
    } else {
        LogBuilder::from(ps.env()).setup(ps.env(), io::stderr());
    }
    if (ps.is_toplevel() || j > 1) && ps.env().locks_broken() {
        log_warn!("detected broken fcntl locks; parallelism disabled.\n");
        log_warn!("  ...details: https://github.com/Microsoft/WSL/issues/1927\n");
        if j > 1 {
            j = 1;
        }
    }

    {
        let mut ptx = ProcessTransaction::new(&mut ps, TransactionBehavior::Deferred)?;
        for t in &targets {
            if Path::new(t).exists() {
                let f = redo::File::from_name(&mut ptx, t, true)?;
                if !f.is_generated() {
                    log_warn!(
                        "{}: exists and not marked as generated; not redoing.\n",
                        f.nice_name(ptx.state().env())?
                    );
                }
            }
        }
    }

    if j < 0 || j > 1000 {
        return Err(format_err!("invalid --jobs value: {}", j));
    }
    let mut server = JobServer::setup(j)?;
    assert!(ps.is_flushed());
    let build_result = server.block_on(builder::run(
        &mut ps,
        &mut server.handle(),
        &targets,
        |_, _| Ok((true, Dirtiness::Dirty)),
    ));
    assert!(ps.is_flushed());
    let return_tokens_result = server.force_return_tokens();
    if let Err(e) = &return_tokens_result {
        log_err!("unexpected error: {}", e);
    }
    build_result.map_err(|e| e.into()).and(return_tokens_result)
}
