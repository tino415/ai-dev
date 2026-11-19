{
  description = "ai-dev, a debug notebook where agents propose blocks and a person approves their run and read";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { nixpkgs, ... }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forEachSystem = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      nixosModules.default = ./nix/module.nix;

      packages = forEachSystem (pkgs: rec {
        ai-dev = pkgs.callPackage ./nix/package.nix { };
        default = ai-dev;
      });

      devShells = forEachSystem (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            cargo
            rustc
            pkg-config
            openssl
            tmux
            chromium
          ];
        };
      });
    };
}
