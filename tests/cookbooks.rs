pub mod cookbook;
extern crate redo;
use cookbook::CookbookTest;
use std::process::Command;
//use redo::tests::*;

fn setup() {
    println!("Setup")
}

fn teardown() {
    println!("Teardown")
}

fn clear_redo_env(cmd: &mut Command) -> &mut Command {
    for (k, _) in std::env::vars_os() {
        if k.to_str()
            .map(|k| k.starts_with("REDO_") || k == "REDO" || k == "MAKEFLAGS" || k == "DO_BUILT")
            .unwrap_or(false)
        {
            cmd.env_remove(k);
        }
    }
    cmd
}

fn main() {
    // Setup test environment
    setup();

    // Run the tests
    for t in inventory::iter::<CookbookTest> {
        (t.test_fn)()
    }

    // Teardown test environment
    teardown();
}