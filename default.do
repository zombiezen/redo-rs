# shellcheck shell=sh
case $1 in
  target/debug/redo)
    redo-always
    cargo build --bin redo
    ;;
  target/debug/redo-always)
    redo-always
    cargo build --bin redo-always
    ;;
  target/release/redo)
    redo-always
    cargo build --release --bin redo
    ;;
  target/release/redo-always)
    redo-always
    cargo build --release --bin redo-always
    ;;
  *)
    echo "no rule to build '$1'" >&2
    exit 1
    ;;
esac
