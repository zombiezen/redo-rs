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

mod always;
mod ifchange;
mod ifcreate;
mod log;
mod ood;
mod sources;
mod stamp;
mod targets;
mod unlocked;
mod whichdo;

use anyhow::{anyhow, Error};
use clap::{crate_version, App, AppSettings, Arg};
use rusqlite::TransactionBehavior;
use std::convert::Infallible;
use std::env;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};

use redo::builder::{self, StdinLogReader, StdinLogReaderBuilder};
use redo::logs::LogBuilder;
use redo::{
    self, log_err, log_warn, Dirtiness, Env, JobServer, OptionalBool, ProcessState,
    ProcessTransaction, RedoErrorKind, RedoPath, EXIT_CYCLIC_DEPENDENCY,
    EXIT_FAILED_IN_ANOTHER_THREAD, EXIT_FAILURE, EXIT_INVALID_TARGET, EXIT_SUCCESS,
};

fn main() {
    let name = env::args_os()
        .nth(0)
        .and_then(|a0| {
            PathBuf::from(a0)
                .file_name()
                .map(|base| base.to_os_string())
        })
        .unwrap_or("redo".into());
    let result = match name.to_str() {
        Some("redo-always") => always::run(),
        Some("redo-ifchange") => ifchange::run(),
        Some("redo-ifcreate") => ifcreate::run(),
        Some("redo-log") => log::run(),
        Some("redo-ood") => ood::run(),
        Some("redo-sources") => sources::run(),
        Some("redo-stamp") => stamp::run(),
        Some("redo-targets") => targets::run(),
        Some("redo-unlocked") => unlocked::run(),
        Some("redo-whichdo") => whichdo::run(),
        _ => run_redo(),
    };
    match result {
        Ok(_) => std::process::exit(EXIT_SUCCESS),
        Err(e) => {
            let msg = {
                use std::fmt::Write;

                let mut s = String::new();
                for e in e.chain() {
                    if !s.is_empty() {
                        write!(s, ": ").unwrap();
                    }
                    write!(s, "{}", e).unwrap();
                }
                s
            };
            log_err!("{}", msg);
            let retcode = match RedoErrorKind::of(&e) {
                RedoErrorKind::FailedInAnotherThread { .. } => EXIT_FAILED_IN_ANOTHER_THREAD,
                RedoErrorKind::InvalidTarget(_) => EXIT_INVALID_TARGET,
                RedoErrorKind::CyclicDependency => EXIT_CYCLIC_DEPENDENCY,
                _ => EXIT_FAILURE,
            };
            std::process::exit(retcode)
        }
    }
}

/// Return the common list of `redo-log` flags.
pub(crate) fn log_flags() -> Vec<clap::Arg<'static, 'static>> {
    vec![
        Arg::from_usage("--no-details").help("only show 'redo' recursion trace, not build output"),
        Arg::from_usage("--details")
            .hidden(true)
            .overrides_with("no-details"),
        Arg::from_usage("--no-status")
            .help("don't display build summary line at the bottom of the screen"),
        Arg::from_usage("--status")
            .hidden(true)
            .overrides_with("no-status"),
        Arg::from_usage("--no-pretty")
            .help("don't pretty-print logs, show raw @@REDO output instead"),
        Arg::from_usage("--pretty")
            .hidden(true)
            .overrides_with("no-pretty"),
        Arg::from_usage("--no-color")
            .help("disable ANSI color; --color to force enable (default: auto)"),
        Arg::from_usage("--color")
            .hidden(true)
            .overrides_with("no-color"),
        Arg::from_usage("--debug-locks 'print messages about file locking (useful for debugging)'"),
        Arg::from_usage("--no-debug-locks")
            .hidden(true)
            .overrides_with("debug-locks"),
        Arg::from_usage(
            "--debug-pids 'print process ids as part of log messages (useful for debugging)'",
        ),
        Arg::from_usage("--no-debug-pids")
            .hidden(true)
            .overrides_with("debug-pids"),
    ]
}

fn run_redo() -> Result<(), Error> {
    let matches = App::new("redo")
        .about("Build the listed targets whether they need it or not.")
        .version(crate_version!())
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
        .args(&log_flags())
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
    if auto_bool_arg(&matches, "keep-going").unwrap_or(false) {
        std::env::set_var("REDO_KEEP_GOING", "1");
    }
    if auto_bool_arg(&matches, "shuffle").unwrap_or(false) {
        std::env::set_var("REDO_SHUFFLE", "1");
    }
    if auto_bool_arg(&matches, "debug-locks").unwrap_or(false) {
        std::env::set_var("REDO_DEBUG_LOCKS", "1");
    }
    if auto_bool_arg(&matches, "debug-pids").unwrap_or(false) {
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
    set_defint("REDO_LOG", auto_bool_arg(&matches, "log"));
    set_defint("REDO_PRETTY", auto_bool_arg(&matches, "pretty"));
    set_defint("REDO_COLOR", auto_bool_arg(&matches, "color"));
    let mut targets = {
        let mut targets = Vec::<&RedoPath>::new();
        for arg in matches.values_of("target").unwrap_or_default() {
            targets.push(RedoPath::from_str(arg)?);
        }
        targets
    };

    let env = Env::init(targets.as_slice())?;
    let mut ps = ProcessState::init(env)?;
    if ps.is_toplevel() && targets.is_empty() {
        targets.push(unsafe { RedoPath::from_str_unchecked("all") });
    }
    let mut j = str::parse::<i32>(matches.value_of("jobs").unwrap_or("0")).unwrap_or(0);
    if ps.is_toplevel() && (ps.env().log().unwrap_or(true) || j > 1) {
        builder::close_stdin()?;
    }
    let mut _stdin_log_reader: Option<StdinLogReader> = None;
    if ps.is_toplevel() && ps.env().log().unwrap_or(true) {
        _stdin_log_reader = Some(
            StdinLogReaderBuilder::from(ps.env())
                .set_status(auto_bool_arg(&matches, "status").unwrap_or(true))
                .set_details(auto_bool_arg(&matches, "details").unwrap_or(true))
                .set_debug_locks(auto_bool_arg(&matches, "debug-locks").unwrap_or(false))
                .set_debug_pids(auto_bool_arg(&matches, "debug-pids").unwrap_or(false))
                .start(ps.env())?,
        );
    } else {
        LogBuilder::from(ps.env()).setup(io::stderr());
    }
    if (ps.is_toplevel() || j > 1) && ps.env().locks_broken() {
        log_warn!("detected broken fcntl locks; parallelism disabled.\n");
        log_warn!("  ...details: https://github.com/Microsoft/WSL/issues/1927\n");
        if j > 1 {
            j = 1;
        }
    }

    {
        let mut ptx = ProcessTransaction::new(&mut ps, TransactionBehavior::Immediate)?;
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
        return Err(anyhow!("invalid --jobs value: {}", j));
    }
    let mut server = JobServer::setup(j)?;
    assert!(ps.is_flushed());
    let build_result = server.block_on(builder::run(
        &mut ps,
        &server.handle(),
        &targets,
        |_, _| -> Result<(bool, Dirtiness), Infallible> { Ok((true, Dirtiness::Dirty)) },
    ));
    assert!(ps.is_flushed());
    let return_tokens_result = server.force_return_tokens();
    if let Err(e) = &return_tokens_result {
        log_err!("unexpected error: {}", e);
    }
    build_result
        .map_err(|e| e.into())
        .and(return_tokens_result.map_err(Into::into))
}

/// Converts an argument pair match of `name` and `"no-" + name` into a tri-state.
pub(crate) fn auto_bool_arg<S: AsRef<str>>(matches: &clap::ArgMatches, name: S) -> OptionalBool {
    let name = name.as_ref();
    const NEGATIVE_PREFIX: &str = "no-";
    let negative_name = {
        let mut negative_name = String::with_capacity(name.len() + NEGATIVE_PREFIX.len());
        negative_name.push_str(NEGATIVE_PREFIX);
        negative_name.push_str(name);
        negative_name
    };
    if matches.is_present(name) {
        OptionalBool::On
    } else if matches.is_present(negative_name) {
        OptionalBool::Off
    } else {
        OptionalBool::Auto
    }
}
