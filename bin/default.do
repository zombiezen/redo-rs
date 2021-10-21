# shellcheck shell=sh
exec >&2
case "$1" in
  redo|redo-*)
    redo-always
    cargo build --release --bin "$1"
    cp --archive "../target/release/$1" "$3"
    ;;
	*) echo "$0: don't know how to build '$1'" >&2; exit 99 ;;
esac
