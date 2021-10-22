use failure::Error;
use rusqlite::TransactionBehavior;
use std::io;
use std::path::PathBuf;

use redo::logs::LogBuilder;
use redo::{self, DepMode, Env, ProcessState, ProcessTransaction, Stamp};

fn main() {
    redo::run_program("redo-always", run);
}

fn run() -> Result<(), Error> {
    let env = Env::inherit()?;
    LogBuilder::from(&env).setup(&env, io::stderr());

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
