# shellcheck shell=sh
exec >&2
redo-ifchange ../Cargo.toml ../Cargo.lock ../src/*.rs ../src/bin/redo/*.rs
cargo build --release --bin redo
cp --archive ../target/release/redo "$3"
