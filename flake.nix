# Copyright 2023 Ross Light
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

{
  description = "Port of apenwarr's redo to Rust";

  outputs = { self, nixpkgs, flake-utils }:
    let
      supportedSystems = [
        flake-utils.lib.system.aarch64-darwin
        flake-utils.lib.system.x86_64-linux
        flake-utils.lib.system.x86_64-darwin
      ];
    in
      flake-utils.lib.eachSystem supportedSystems (system:
      let
        pkgs = import nixpkgs { inherit system; };
      in {
        packages.default = pkgs.callPackage ./redo-zombiezen.nix {};

        packages.rustLibSrc = pkgs.rust.packages.stable.rustPlatform.rustLibSrc;

        apps.default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/redo";
        };

        devShells.default = pkgs.mkShell {
          packages = [
            pkgs.bash
            pkgs.python310
            pkgs.rust-analyzer
            pkgs.rustfmt
            pkgs.rust.packages.stable.rustPlatform.rustLibSrc
          ];
          inputsFrom = [self.packages.${system}.default];
        };
      });
}
