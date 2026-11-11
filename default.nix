{pkgs ? import <nixpkgs> {}}: let
  manifest = (pkgs.lib.importTOML ./Cargo.toml).workspace.package;
in
  pkgs.rustPlatform.buildRustPackage {
    pname = "chainz";
    version = manifest.version;
    cargoLock.lockFile = ./Cargo.lock;
    # Some crates look for pkg-config to locate openssl.
    nativeBuildInputs = [
      pkgs.pkg-config
    ];

    # Provide OpenSSL to the build environment.
    buildInputs = [
      pkgs.openssl
    ];
    src = pkgs.lib.cleanSource ./.;
  }
