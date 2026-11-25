# The crack package: `nix build` (see flake.nix for the build types).
{ lib
, rustPlatform
, pkg-config
, makeWrapper
, cmake
, perl
, freetype
, fontconfig
, libxcb
, libxkbcommon
, wayland
, vulkan-loader
, libx11
, libxcursor
, libxi
, libxrandr
, git
, ripgrep
, bubblewrap
  # Cargo features; the default build has every provider and language.
, features ? [ ]
, noDefaultFeatures ? false
, flavor ? "default"
}:

let
  # Libraries gpui links against or opens by name at runtime (as in shell.nix).
  runtimeLibs = [
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
  cargoToml = lib.importTOML ../Cargo.toml;
in
rustPlatform.buildRustPackage {
  pname = "crack" + lib.optionalString (flavor != "default") "-${flavor}";
  version = cargoToml.package.version;

  src = lib.cleanSourceWith {
    src = ./..;
    # Only what the build needs: no build output, worktrees or private notes.
    filter = path: type:
      !(builtins.elem (baseNameOf path) [ "target" "result" ".claude" ".git" ".github" "PLAN.md" ]);
  };

  cargoLock.lockFile = ../Cargo.lock;

  buildNoDefaultFeatures = noDefaultFeatures;
  buildFeatures = features;

  nativeBuildInputs = [ pkg-config makeWrapper cmake perl ];
  buildInputs = runtimeLibs;

  # The tests need git, jq and a writable home; `just check` runs them.
  doCheck = false;

  postInstall = ''
    install -Dm644 packaging/crack.desktop $out/share/applications/crack.desktop
    install -Dm644 assets/logo.png $out/share/pixmaps/crack.png
    install -Dm644 LICENSE $out/share/licenses/crack/LICENSE
  '';

  # Libraries opened by name at runtime must be findable; shell commands get git,
  # ripgrep and bubblewrap (the sandbox fallback on kernels without Landlock).
  postFixup = ''
    wrapProgram $out/bin/crack \
      --prefix LD_LIBRARY_PATH : ${lib.makeLibraryPath runtimeLibs} \
      --suffix PATH : ${lib.makeBinPath [ git ripgrep bubblewrap ]}
  '';

  meta = {
    description = cargoToml.package.description;
    homepage = cargoToml.package.repository;
    license = lib.licenses.mit;
    mainProgram = "crack";
    platforms = lib.platforms.linux;
  };
}
