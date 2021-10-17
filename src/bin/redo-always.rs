use failure::Error;
use rusqlite::TransactionBehavior;
use std::path::PathBuf;
use std::process;

use redo::{self, DepMode, Env, ProcessState, ProcessTransaction, Stamp};

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
            process::exit(1);
        }
    }
}

fn run() -> Result<(), Error> {
    let env = Env::inherit()?;
    // TODO(soon): logs::setup

    let mut me = PathBuf::new();
    me.push(env.startdir());
    me.push(env.pwd());
    me.push(env.target());
    let mut ps = ProcessState::init(env)?;
    let mut ptx = ProcessTransaction::new(&mut ps, TransactionBehavior::Deferred)?;
    let mut f = redo::File::from_name(
        &mut ptx,
        me.as_os_str()
            .to_str()
            .expect("invalid character in target name"),
        true,
    )?;
    f.add_dep(&mut ptx, DepMode::Modified, redo::ALWAYS)?;
    let mut always = redo::File::from_name(&mut ptx, redo::ALWAYS, true)?;
    always.set_stamp(Stamp::MISSING);
    always.set_changed(ptx.state().env());
    always.save(&mut ptx)?;
    ptx.commit()?;
    Ok(())
}
