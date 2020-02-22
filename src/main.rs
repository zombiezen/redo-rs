use failure::Error;

mod env;
mod helpers;
mod state;

fn main() -> Result<(), Error> {
    let (e, toplevel) = env::init(&[] as &[&str])?;
    println!("is_toplevel = {}", toplevel);
    println!("{:?}", e);
    let manager = state::LockManager::open("locks")?;
    println!("locks broken: {}", manager.detect_broken_locks()?);
    Ok(())
}
