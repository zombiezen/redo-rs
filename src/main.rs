use clap::{App, AppSettings, Arg};
use failure::{format_err, Error};
use rusqlite::TransactionBehavior;
use std::ffi::OsString;
use std::io;
use std::path::Path;

use redo::builder::{self, StdinLogReader, StdinLogReaderBuilder};
use redo::logs::LogBuilder;
use redo::{self, log_err, log_warn, Env, JobServer, ProcessState, ProcessTransaction};

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
        .arg(
            Arg::from_usage("--no-log")
                .help("don't capture error output, just let it flow straight to stderr"),
        )
        .arg(
            Arg::from_usage("--log")
                .hidden(true)
                .overrides_with("no-log"),
        )
        .arg(
            Arg::from_usage("--no-pretty")
                .help("don't pretty-print logs, show raw @@REDO output instead"),
        )
        .arg(
            Arg::from_usage("--pretty")
                .hidden(true)
                .overrides_with("no-pretty"),
        )
        .arg(
            Arg::from_usage("--no-color")
                .help("disable ANSI color; --color to force enable (default: auto)"),
        )
        .arg(
            Arg::from_usage("--color")
                .hidden(true)
                .overrides_with("no-color"),
        )
        .arg(Arg::from_usage(
            "--debug-locks 'print messages about file locking (useful for debugging)'",
        ))
        .arg(Arg::from_usage(
            "--debug-pids 'print process ids as part of log messages (useful for debugging)'",
        ))
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
    if matches.is_present("debug-locks") {
        std::env::set_var("REDO_DEBUG_LOCKS", "1");
    }
    if matches.is_present("debug-pids") {
        std::env::set_var("REDO_DEBUG_PIDS", "1");
    }
    std::env::set_var(
        "REDO_LOG",
        std::env::var_os("REDO_LOG").unwrap_or_else(|| {
            OsString::from(if matches.is_present("no-log") {
                "0"
            } else if !matches.is_present("log") {
                "1"
            } else {
                "2"
            })
        }),
    );
    std::env::set_var(
        "REDO_PRETTY",
        std::env::var_os("REDO_PRETTY").unwrap_or_else(|| {
            OsString::from(if matches.is_present("no-pretty") {
                "0"
            } else if !matches.is_present("pretty") {
                "1"
            } else {
                "2"
            })
        }),
    );
    std::env::set_var(
        "REDO_COLOR",
        std::env::var_os("REDO_COLOR").unwrap_or_else(|| {
            OsString::from(if matches.is_present("no-color") {
                "0"
            } else if !matches.is_present("color") {
                "1"
            } else {
                "2"
            })
        }),
    );
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
    if ps.is_toplevel() && (ps.env().log() != 0 || j > 1) {
        builder::close_stdin()?;
    }
    let mut _stdin_log_reader: Option<StdinLogReader> = None;
    if ps.is_toplevel() && ps.env().log() != 0 {
        _stdin_log_reader = Some(StdinLogReaderBuilder::from(ps.env()).start(ps.env())?);
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
    let mut server = JobServer::setup(1)?;
    assert!(ps.is_flushed());
    let build_result = server.block_on(builder::run(&mut ps, &mut server.handle(), &targets));
    assert!(ps.is_flushed());
    let return_tokens_result = server.force_return_tokens();
    if let Err(e) = &return_tokens_result {
        log_err!("unexpected error: {}", e);
    }
    build_result.map_err(|e| e.into()).and(return_tokens_result)
}
