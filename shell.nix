{pkgs ? import <nixpkgs> {}}:
pkgs.mkShell {
  # Get dependencies from the main package
  inputsFrom = [(pkgs.callPackage ./default.nix {})];
  # Additional tooling
  buildInputs = with pkgs;
    [
      rust-analyzer # LSP Server
      rustfmt # Formatter
      clippy # Linter
      foundry-bin
      hyperfine # performance gate (scripts/perf-gate.sh)
      jq # performance gate
    ]
    ++ lib.optional stdenv.isLinux strace; # exec structural test
}
