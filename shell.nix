{pkgs ? import <nixpkgs> {}}:
pkgs.mkShell {
  # Get dependencies from the main package
  inputsFrom = [(pkgs.callPackage ./default.nix {})];
  # Additional tooling
  buildInputs = with pkgs;
    [
      openssl
      pkg-config
      rust-analyzer # LSP Server
      rustfmt # Formatter
      clippy # Linter
      foundry-bin
      hyperfine # performance gate (scripts/perf-gate.sh)
    ]
    ++ lib.optional stdenv.isLinux strace; # exec structural test
}
