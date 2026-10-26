{
  description = "herdr-warp: herdr agent notifications and tab titles, bridged to Warp";

  inputs = {
    # The nixpkgs revision nix-config pins (its flake.lock, nixos-26.05), so
    # swapping this package in rebuilds nothing else.
    nixpkgs.url = "github:NixOS/nixpkgs/1f607aa79b6356ed45d3cd3c8079c196e79efa2c";
    systems.url = "github:nix-systems/default";
    flake-utils = {
      url = "github:numtide/flake-utils";
      inputs.systems.follows = "systems";
    };
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      rust-overlay,
      ...
    }:
    # The VM half runs on x86_64-linux, the launchd agent on aarch64-darwin.
    flake-utils.lib.eachSystem
      [
        "x86_64-linux"
        "aarch64-darwin"
      ]
      (
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
          };
          # The toolchain rust-toolchain.toml pins, for the build, the checks
          # and the dev shell alike.
          rust = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
          rustPlatform = pkgs.makeRustPlatform {
            cargo = rust;
            rustc = rust;
          };
          # Editor tooling for the dev shell only. Same version as rustc, so
          # rust-analyzer's proc-macro server matches the compiler.
          rust-dev = rust.override {
            extensions = [
              "rust-analyzer"
              "rust-src"
            ];
          };
          # The CLI tools CI runs besides Rust itself, pinned by flake.lock.
          # Workflows put them on PATH through .github/actions/setup-tools;
          # the dev shell gets the same set.
          ciTools = [ pkgs.actionlint ];

          # The legacy command names the nix-config modules use; each is a
          # symlink to the one binary, which picks the subcommand from argv[0].
          legacyNames = [
            "herdr-notify-hook"
            "herdr-notify-serve"
            "herdr-notify-agent"
            "herdr-tab-title-hook"
          ];
          src = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./src
              ./tests
            ];
          };

          herdr-warp = rustPlatform.buildRustPackage {
            pname = "herdr-warp";
            version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).package.version;
            inherit src;
            cargoLock.lockFile = ./Cargo.lock;
            postInstall = ''
              for n in ${builtins.concatStringsSep " " legacyNames}; do
                ln -s herdr-warp $out/bin/$n
              done
            '';
            meta = {
              description = "herdr agent notifications on the Mac via Warp, and agent tab titles";
              license = pkgs.lib.licenses.mit;
              platforms = pkgs.lib.platforms.unix;
              mainProgram = "herdr-warp";
            };
          };

          # A cargo command over the package's source and vendored crates,
          # with nothing installed.
          cargoCheck =
            name: cmd:
            herdr-warp.overrideAttrs {
              pname = "herdr-warp-${name}";
              buildPhase = ''
                runHook preBuild
                ${cmd}
                runHook postBuild
              '';
              doCheck = false;
              installPhase = "touch $out";
              postInstall = "";
            };
        in
        {
          formatter = pkgs.nixfmt;

          packages.default = herdr-warp;

          packages.ci-tools = pkgs.buildEnv {
            name = "herdr-warp-ci-tools";
            paths = ciTools;
            pathsToLink = [ "/bin" ];
          };

          checks = {
            # buildRustPackage runs `cargo test` in its checkPhase.
            test = self.packages.${system}.default;
            clippy = cargoCheck "clippy" "cargo clippy --offline --all-targets -- -D warnings";
            fmt = cargoCheck "fmt" "cargo fmt --check";
          };

          devShells.default = pkgs.mkShell {
            name = "rust-dev";
            packages = [
              rust-dev
              pkgs.bashInteractive
              pkgs.nixfmt
              # parity/test_parity.py, against the Python originals.
              pkgs.python3
            ]
            ++ ciTools;
          };
        }
      );
}
