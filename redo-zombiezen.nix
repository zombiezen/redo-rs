# Copyright 2022 Ross Light
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

{ lib, nix-gitignore, rustPlatform }:

rustPlatform.buildRustPackage rec {
  pname = "redo-zombiezen";
  version = "0.2.2-alpha1";

  src = let
    root = ./.;
    patterns = nix-gitignore.withGitignoreFile extraIgnores root;
    extraIgnores = ["/t/" "/redo/" "*.nix"];
  in builtins.path {
    name = "redo-zombiezen";
    path = root;
    filter = nix-gitignore.gitignoreFilterPure (_: _: true) patterns root;
  };
  cargoLock = {
    lockFile = ./Cargo.lock;
  };
  cargoTestFlags = ["--lib" "--bins" "--examples"];

  postInstall = ''
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
      ln -s redo "$out/bin/$name"
    done
  '';

  meta = with lib; {
    description = "Port of apenwarr's redo to Rust";
    homepage = "https://github.com/zombiezen/redo-rs";
    license = licenses.asl20;
    mainProgram = "redo";
    platforms = platforms.linux ++ platforms.darwin;
  };
}
