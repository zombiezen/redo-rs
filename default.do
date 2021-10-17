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
  target/debug/redo-ifchange)
    redo-always
    cargo build --bin redo-ifchange
    ;;
  target/release/redo)
    redo-always
    cargo build --release --bin redo
    ;;
  target/release/redo-always)
    redo-always
    cargo build --release --bin redo-always
    ;;
  target/release/redo-ifchange)
    redo-always
    cargo build --release --bin redo-ifchange
    ;;
  *)
    echo "no rule to build '$1'" >&2
    exit 1
    ;;
esac
