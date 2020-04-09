# shellcheck shell=sh
case $1 in
  target/debug/redo-rs)
    redo-always
    cargo build
    ;;
  target/release/redo-rs)
    redo-always
    cargo build --release
    ;;
  *)
    echo "no rule to build '$1'" >&2
    exit 1
    ;;
esac
