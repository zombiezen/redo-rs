# shellcheck shell=sh
exec >&2
redo-ifchange \
  Cargo.lock \
  Cargo.toml \
  src/*.rs \
  src/bin/redo/*.rs
cargo build
