use failure::Error;
use rusqlite::TransactionBehavior;
use std::path::Path;
use std::process;

mod env;
mod helpers;
mod state;

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
            eprintln!("{}: {:?}", name, e);
            process::exit(1);
        }
    }
}

fn run() -> Result<(), Error> {
    let targets: Vec<String> = std::env::args().skip(1).collect();
    let mut ps = state::ProcessState::init(targets.as_slice())?;

    {
        let mut ptx = state::ProcessTransaction::new(&mut ps, TransactionBehavior::Deferred)?;
        for t in &targets {
            if Path::new(t).exists() {
                let f = state::File::from_name(&mut ptx, t, true)?;
                if !f.is_generated {
                    // TODO(soon): log as warn.
                    eprintln!(
                        "{}: exists and not marked as generated; not redoing",
                        f.nice_name(ptx.state().env())?
                    );
                }
            }
        }
    }
    Ok(())
}
