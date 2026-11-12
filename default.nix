{pkgs ? import <nixpkgs> {}}: let
  manifest = (pkgs.lib.importTOML ./Cargo.toml).workspace.package;
  fs = pkgs.lib.fileset;
in
  pkgs.rustPlatform.buildRustPackage {
    pname = "chainz";
    version = manifest.version;
    cargoLock.lockFile = ./Cargo.lock;
    # Only what cargo needs: never copy target/ or .worktrees/ into the store.
    src = fs.toSource {
      root = ./.;
      fileset = fs.unions [
        ./Cargo.toml
        ./Cargo.lock
        ./README.md # the chainz crate's readme (crates/chainz/Cargo.toml)
        ./crates
      ];
    };
    cargoBuildFlags = ["-p" "chainz"];
    # The integration tests spawn anvil and bind sockets; CI runs them.
    doCheck = false;
    # keyring's vendored libdbus is built from source; its build script
    # still probes with pkg-config.
    nativeBuildInputs = [pkgs.pkg-config];
    meta.mainProgram = "chainz";
  }
