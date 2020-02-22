use failure::Error;
use std::process;

mod env;
mod helpers;
mod state;

fn main() {
    match run() {
        Ok(_) => {},
        Err(e) => {
            let name: String = match std::env::current_exe() {
                Ok(exe) => exe.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| String::from("redo")),
                Err(_) => String::from("redo"),
            };
            eprintln!("{}: {}", name, e);
            process::exit(1);
        },
    }
}

fn run() -> Result<(), Error> {
    let targets: Vec<String> = std::env::args().collect();
    let ps = state::ProcessState::init(targets.as_slice())?;

    println!("{:?}", ps.env());
    Ok(())
}
