# shellcheck shell=sh
exec >&2
case "$1" in
  redo|redo-*)
    mainfile="../src/bin/${1}.rs"
    if [ "$1" = redo ]; then
      mainfile=../src/main.rs
    fi
    for name in ../src/*.rs; do
      if [ "$name" != ../src/main.rs ]; then
        echo "$name"
      fi
    done |
    xargs redo-ifchange ../Cargo.lock ../Cargo.toml "$mainfile"

    cargo build --release --bin "$1"
    cp --archive "../target/release/$1" "$3"
    ;;
	*) echo "$0: don't know how to build '$1'" >&2; exit 99 ;;
esac
