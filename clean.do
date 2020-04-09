# shellcheck shell=sh
exec >&2
redo-always
cargo clean
[ -z "$DO_BUILT" ] && rm -rf .do_built .do_built.dir
find . -name '*.tmp' -exec rm -f {} \;
find . -name '*.did' -exec rm -f {} \;
