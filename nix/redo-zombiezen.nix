{ lib, nix-gitignore, rustPlatform, }:

rustPlatform.buildRustPackage rec {
  pname = "redo-zombiezen";
  version = "0.2.2-alpha1";

  src = nix-gitignore.gitignoreSource ["/t/" "/redo/" "*.nix" "/nix/"] ../.;
  cargoLock = {
    lockFile = ../Cargo.lock;
  };
  cargoTestFlags = ["--lib" "--bins" "--examples"];

  meta = with lib; {
    description = "Port of apenwarr's redo to Rust";
    homepage = "https://github.com/zombiezen/redo-rs";
    license = licenses.asl20;
    mainProgram = "redo";
    platforms = platforms.linux ++ platforms.darwin;
  };
}
