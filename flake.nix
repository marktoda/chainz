{
  description = "Chainz";
  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-24.11";
  };
  outputs = {
    self,
    nixpkgs,
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
          inherit system;
          pkgs = nixpkgs.legacyPackages.${system};
        });
  in {
    packages = forAllSystems ({pkgs, ...}: {
      default = pkgs.callPackage ./default.nix {};
    });
    devShells = forAllSystems ({pkgs, ...}: {
      default = pkgs.callPackage ./shell.nix {};
    });
  };
}
