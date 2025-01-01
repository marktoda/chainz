{pkgs ? import <nixpkgs> {}}:
pkgs.mkShell {
  # Get dependencies from the main package
  inputsFrom = [(pkgs.callPackage ./default.nix {})];
  # Additional tooling
  buildInputs = with pkgs; [
    openssl
    pkg-config
    rust-analyzer # LSP Server
    rustfmt # Formatter
    clippy # Linter
    foundry-bin
  ];
}
