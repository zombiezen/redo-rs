use super::CookbookTest;
use crate::clear_redo_env;
use std::process::Command;

// The test mimicks the upstream documented usage.
fn hello_test() {
    let crate_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let redo_path = std::path::Path::new(env!("CARGO_BIN_EXE_redo"));
    let cd = crate_dir.join("cookbook").join("hello");

    let status = clear_redo_env(&mut Command::new(redo_path))
        .current_dir(cd.clone())
        .arg("hello")
        .env("RUST_BACKTRACE", "1")
        .spawn()
        .expect("could not complete redo build")
        .wait()
        .expect("could not get exit status");
    assert!(status.success(), "hello build status = {:?}", status);
}

fn hello_world_test() {
    let crate_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let redo_path = std::path::Path::new(env!("CARGO_BIN_EXE_redo"));
    let cd = crate_dir.join("cookbook").join("hello");

    let status = clear_redo_env(&mut Command::new(
        crate_dir.join("cookbook").join("hello").join("hello"),
    ))
    .current_dir(crate_dir.join("cookbook").join("hello"))
    .spawn()
    .expect("could not run hello")
    .wait()
    .expect("could not get exit status");
    assert!(status.success(), "hello world status = {:?}", status);
}

// Submit order matters
inventory::submit!(CookbookTest {
    name: "hello_world",
    test_fn: hello_world_test
});

inventory::submit!(CookbookTest {
    name: "hello",
    test_fn: hello_test
});
