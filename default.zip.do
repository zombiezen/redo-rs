# shellcheck shell=sh
# shellcheck disable=SC2164
#
# Copyright 2021 Ross Light
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     https://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.
#
# SPDX-License-Identifier: Apache-2.0

exec >&2

case "$1" in
  redo_*) ;;
  *) echo "$0: don't know how to build '$1'" >&2; exit 99 ;;
esac

redo-ifchange \
  CHANGELOG.md \
  Cargo.lock \
  Cargo.toml \
  LICENSE \
  README.md \
  src/*.rs \
  src/bin/redo/*.rs
cargo build --release

mytmpdir="$(mktemp -d 2>/dev/null || mktemp -d -t 'redo')"
trap 'rm -rf "$mytmpdir"' EXIT

root="$mytmpdir/$2"
mkdir "$root" "$root/bin"
cp target/release/redo "$root/bin/redo"
chmod +x "$root/bin/redo"
for name in \
    redo-always \
    redo-ifchange \
    redo-ifcreate \
    redo-log \
    redo-ood \
    redo-sources \
    redo-stamp \
    redo-targets \
    redo-unlocked \
    redo-whichdo; do
  ln -s redo "$root/bin/$name"
done
cp CHANGELOG.md LICENSE README.md "$root/"
out="$(pwd)/$3"
cd "$mytmpdir"
zip --recurse-paths --symlinks "$out" "$(basename "$root")"
