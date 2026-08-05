{
  description = "observepass completion-observation research";
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    fenix = { url = "github:nix-community/fenix"; inputs.nixpkgs.follows = "nixpkgs"; };
  };
  outputs = { nixpkgs, flake-utils, fenix, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        stable = fenix.packages.${system}.fromToolchainFile {
          file = ./rust-toolchain.toml;
          sha256 = "sha256-mvUGEOHYJpn3ikC5hckneuGixaC+yGrkMM/liDIDgoU=";
        };
        nightly = fenix.packages.${system}.latest.withComponents [ "cargo" "rustc" "rust-src" "rust-std" "miri" ];
      in {
        devShells.default = pkgs.mkShell { packages = [ stable pkgs.cargo-nextest ]; };
        devShells.miri = pkgs.mkShell { packages = [ nightly ]; };
      });
}
