{
  description = "A CLI for reading, searching, syncing, and automating Confluence";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  };

  outputs =
    { self, nixpkgs }:
    let
      inherit (nixpkgs) lib;

      # x86_64-darwin is omitted because nixpkgs-unstable (26.11) no longer
      # evaluates for it; Intel Mac users can override the `nixpkgs` input to
      # a 26.05 branch, which still supports it.
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];

      forAllSystems = f: lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});

      cargoToml = lib.importTOML ./Cargo.toml;
    in
    {
      overlays.default = final: prev: {
        confluence-cli = final.callPackage (
          {
            lib,
            stdenv,
            rustPlatform,
            installShellFiles,
            versionCheckHook,
          }:
          rustPlatform.buildRustPackage (finalAttrs: {
            pname = "confluence-cli";
            version = cargoToml.package.version;

            src = lib.fileset.toSource {
              root = ./.;
              fileset = lib.fileset.unions [
                ./Cargo.toml
                ./Cargo.lock
                ./README.md
                ./CHANGELOG.md
                ./LICENSE
                ./assets
                ./docs
                ./src
                ./tests
              ];
            };

            cargoLock.lockFile = ./Cargo.lock;

            nativeBuildInputs = [
              installShellFiles
              versionCheckHook
            ];

            # The end-to-end suite is `#[ignore]`d and needs a live Confluence
            # instance; everything else runs against an in-process simulator.
            cargoTestFlags = [ "--all-targets" ];

            postInstall =
              # html2md is a cdylib+rlib crate; the binary links it statically, so
              # the .so cargo emits alongside it is dead weight.
              ''
                rm -rf $out/lib
              ''
              + lib.optionalString (stdenv.buildPlatform.canExecute stdenv.hostPlatform) ''
                for shell in bash fish zsh; do
                  $out/bin/confluence completions "$shell" > "confluence.$shell"
                done
                installShellCompletion confluence.{bash,fish,zsh}
              '';

            doInstallCheck = true;
            versionCheckProgramArg = "--version";

            meta = {
              description = cargoToml.package.description;
              homepage = cargoToml.package.homepage;
              changelog = "https://github.com/rvben/confluence-cli/blob/v${finalAttrs.version}/CHANGELOG.md";
              license = lib.licenses.mit;
              mainProgram = "confluence";
              platforms = lib.platforms.unix;
            };
          })
        ) { };
      };

      packages = forAllSystems (pkgs: rec {
        confluence-cli = (pkgs.extend self.overlays.default).confluence-cli;
        default = confluence-cli;
      });

      checks = forAllSystems (pkgs: {
        inherit (self.packages.${pkgs.stdenv.hostPlatform.system}) confluence-cli;
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.confluence-cli ];
          packages = with pkgs; [
            cargo
            rustc
            clippy
            rustfmt
            rust-analyzer
            cargo-nextest
          ];
          env.RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
