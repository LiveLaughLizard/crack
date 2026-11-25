{
  description = "crack: a GPUI coding agent";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { nixpkgs, ... }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      crack = pkgs: args: pkgs.callPackage ./packaging/default.nix args;
    in
    {
      # nix build              all providers (the default)
      # nix build .#no-china   without the Chinese model clouds
      # nix build .#airgap     local and self-hosted servers only
      # nix build .#strict     no YOLO mode, sandbox always on
      packages = forAllSystems (pkgs: {
        default = crack pkgs { };
        no-china = crack pkgs {
          flavor = "no-china";
          noDefaultFeatures = true;
          features = [ "cloud" "yolo" "languages" ];
        };
        airgap = crack pkgs {
          flavor = "airgap";
          noDefaultFeatures = true;
          features = [ "yolo" "languages" ];
        };
        strict = crack pkgs {
          flavor = "strict";
          noDefaultFeatures = true;
          features = [ "cloud" "china" "force-sandbox" "languages" ];
        };
      });

      devShells = forAllSystems (pkgs: {
        default = import ./shell.nix { inherit pkgs; };
      });
    };
}
