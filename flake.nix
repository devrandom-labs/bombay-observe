{
  description = "Generation-safe completion observation for Rust";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    utils.url = "github:numtide/flake-utils";
    crane.url = "github:ipetkov/crane";
    fenix = {
      url = "github:nix-community/fenix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    advisory-db = {
      url = "github:rustsec/advisory-db";
      flake = false;
    };
  };

  outputs = { self, nixpkgs, utils, crane, fenix, advisory-db, ... }:
    utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        rustToolchain = fenix.packages.${system}.fromToolchainFile {
          file = ./rust-toolchain.toml;
          sha256 = "sha256-gh/xTkxKHL4eiRXzWv8KP7vfjSk61Iq48x47BEDFgfk=";
        };
        craneLib = (crane.mkLib pkgs).overrideToolchain (_: rustToolchain);
        src = craneLib.cleanCargoSource ./.;
        commonArgs = {
          inherit src;
          strictDeps = true;
        };
        cargoArtifacts = craneLib.buildDepsOnly commonArgs;
        bombayObserve = craneLib.buildPackage (commonArgs // {
          inherit cargoArtifacts;
          cargoExtraArgs = "--package bombay-observe";
        });
      in {
        checks = {
          bombay-observe = bombayObserve;
          bombay-observe-tests = craneLib.cargoNextest (commonArgs // {
            inherit cargoArtifacts;
            cargoNextestExtraArgs = "--workspace --all-targets";
          });
          bombay-observe-doc = craneLib.cargoDoc (commonArgs // {
            inherit cargoArtifacts;
            cargoDocExtraArgs = "--workspace --no-deps";
          });
          bombay-observe-fmt = craneLib.cargoFmt { inherit src; };
          bombay-observe-toml-fmt = craneLib.taploFmt {
            src = pkgs.lib.sources.sourceFilesBySuffices src [ ".toml" ];
          };
          bombay-observe-audit = craneLib.cargoAudit { inherit src advisory-db; };
          bombay-observe-deny = craneLib.cargoDeny { inherit src; };
        };

        packages.default = bombayObserve;

        devShells.default = craneLib.devShell {
          checks = self.checks.${system};
          shellHook = ''
            git config core.hooksPath .githooks
            REPO_NAME=$(basename "$PWD")
            PROPER_REPO_NAME=$(echo "$REPO_NAME" | awk '{print toupper(substr($0,1,1)) substr($0,2)}')
            figlet -f doom "$PROPER_REPO_NAME" | lolcat -a -d 2
            cowsay -f dragon-and-cow "Welcome to the $PROPER_REPO_NAME development environment on ${system}!" | lolcat
          '';
          packages = with pkgs; [
            fenix.packages.${system}.rust-analyzer
            bacon
            cargo-nextest
            cargo-edit
            cargo-deny
            cargo-audit
            just
            taplo
            figlet
            lolcat
            cowsay
            tmux
            tree
            cloc
            gh
          ];
        };
      });
}
