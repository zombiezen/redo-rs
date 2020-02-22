use failure::Error;

mod env;
mod helpers;
mod state;

fn main() -> Result<(), Error> {
    let manager = state::LockManager::open("locks")?;
    println!("locks broken: {}", manager.detect_broken_locks()?);
    Ok(())
}
