use failure::{format_err, Error};
use rusqlite::TransactionBehavior;
use std::io;
use std::path::Path;
use std::process;

use redo::builder::{self, BuildError};
use redo::{self, Env, JobServer, ProcessState, ProcessTransaction};

fn main() {
    match run() {
        Ok(_) => {}
        Err(e) => {
            let name: String = match std::env::current_exe() {
                Ok(exe) => exe
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_else(|| String::from("redo")),
                Err(_) => String::from("redo"),
            };
            if let Some(bt) = e
                .iter_chain()
                .filter_map(|f| f.backtrace().filter(|bt| !bt.is_empty()))
                .last()
            {
                eprint!("Backtrace:\n{}\n\n", bt);
            }
            let msg = e.iter_chain().fold(String::new(), |mut s, e| {
                use std::fmt::Write;
                if !s.is_empty() {
                    write!(s, ": ").unwrap();
                }
                write!(s, "{}", e).unwrap();
                s
            });
            eprintln!("{}: {}", name, msg);
            let retcode = e
                .downcast_ref::<BuildError>()
                .map(|be| i32::from(be.kind()))
                .unwrap_or(1);
            process::exit(retcode);
        }
    }
}

fn run() -> Result<(), Error> {
    use std::os::unix::io::AsRawFd;

    let mut targets: Vec<String> = std::env::args().skip(1).collect();
    let env = Env::init(targets.as_slice())?;
    let mut ps = ProcessState::init(env)?;
    if ps.is_toplevel() && targets.is_empty() {
        targets.push(String::from("all"));
    }
    let mut j = 0; // TODO(soon): Parse -j flag.
    if ps.is_toplevel() && (ps.env().log() != 0 || j > 1) {
        builder::close_stdin()?;
    }
    // TODO(someday): start_stdin_log_reader or logs.setup
    if (ps.is_toplevel() || j > 1) && ps.env().locks_broken() {
        // TODO(soon): log as warn.
        eprintln!("detected broken fcntl locks; parallelism disabled.");
        eprintln!("  ...details: https://github.com/Microsoft/WSL/issues/1927");
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
                    // TODO(soon): log as warn.
                    eprintln!(
                        "{}: exists and not marked as generated; not redoing",
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
    let log_reader_result = if ps.is_toplevel() {
        builder::await_log_reader(ps.env(), io::stderr().as_raw_fd())
    } else {
        Ok(())
    };
    build_result
        .map_err(|e| e.into())
        .and(return_tokens_result)
        .and(log_reader_result)
}
