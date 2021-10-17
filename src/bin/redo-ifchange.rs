use failure::Error;
use rusqlite::TransactionBehavior;
use std::io;
use std::path::PathBuf;
use std::process;

use redo::builder::{self, BuildError};
use redo::{self, DepMode, Env, JobServer, ProcessState, ProcessTransaction};

fn main() {
    match run() {
        Ok(_) => {}
        Err(e) => {
            let name: String = match std::env::current_exe() {
                Ok(exe) => exe
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_else(|| String::from("redo-ifchange")),
                Err(_) => String::from("redo-ifchange"),
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
    if ps.is_toplevel() && ps.env().log() != 0 {
        builder::close_stdin()?;
        // TODO(someday): start_stdin_log_reader or logs.setup
    }

    let mut server;
    {
        let mut ptx = ProcessTransaction::new(&mut ps, TransactionBehavior::Deferred)?;
        let f = if !ptx.state().env().target().as_os_str().is_empty()
            && !ptx.state().env().is_unlocked()
        {
            let mut me = PathBuf::new();
            me.push(ptx.state().env().startdir());
            me.push(ptx.state().env().pwd());
            me.push(ptx.state().env().target());
            Some(redo::File::from_name(
                &mut ptx,
                me.as_os_str().to_str().expect("invalid target name"),
                true,
            )?)
        } else {
            None
        };
        server = JobServer::setup(0)?;
        if let Some(mut f) = f {
            for t in targets.iter() {
                f.add_dep(&mut ptx, DepMode::Modified, t)?;
            }
            f.save(&mut ptx)?;
            ptx.commit()?;
        }
    }

    let build_result = server.block_on(builder::run(&mut ps, &mut server.handle(), &targets));
    // TODO(someday): In the original, there's a state.rollback call.
    // Unclear what this is trying to do.
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
