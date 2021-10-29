# shellcheck shell=sh
exec >&2
case "$1" in
  redo-*)
    redo-ifchange redo
    ln -s redo "$3"
    ;;
	*) echo "$0: don't know how to build '$1'" >&2; exit 99 ;;
esac
