# Development shell for crack (a GPUI coding agent).
#   nix-shell            (or `nix develop` via flake.nix)
{ pkgs ? import <nixpkgs> { } }:

let
  # Libraries gpui links against (freetype, libxcb, libxkbcommon) or opens by
  # name at runtime: the Wayland client, fontconfig, and the Vulkan loader
  # (gpui renders with Vulkan on Linux).
  runtimeLibs = with pkgs; [
    freetype
    fontconfig
    libxcb
    libxkbcommon
    wayland
    vulkan-loader
    libx11
    libxcursor
    libxi
    libxrandr
  ];
in
pkgs.mkShell {
  name = "crack";

  packages = with pkgs; [
    # rustup reads rust-toolchain.toml.
    rustup
    pkg-config
    git
    ripgrep
    just
  ] ++ runtimeLibs;

  # Libraries opened by name at runtime have to be findable.
  LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath runtimeLibs;
}
