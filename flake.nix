{
  description = "Chainz";
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-24.11";
    foundry.url = "github:shazow/foundry.nix";
  };
  outputs = {
    self,
    nixpkgs,
    foundry,
  }: let
    systems = [
      "aarch64-linux"
      "i686-linux"
      "x86_64-linux"
      "aarch64-darwin"
      "x86_64-darwin"
    ];
    forAllSystems = f:
      nixpkgs.lib.genAttrs systems (system:
        f {
          pkgs = import nixpkgs {
            inherit system;
            overlays = [foundry.overlay];
          };
        });
  in {
    packages = forAllSystems ({pkgs, ...}: {
      default = pkgs.callPackage ./default.nix {};
    });
    devShells = forAllSystems ({pkgs, ...}: {
      default = pkgs.callPackage ./shell.nix {inherit pkgs;};
    });
  };
}
