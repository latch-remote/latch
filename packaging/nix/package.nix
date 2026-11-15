# Latch built from this source tree.
{
  lib,
  callPackage,
  rustPlatform,
  pkg-config,
  addDriverRunpath,
  copyDesktopItems,
  makeDesktopItem,
  alsa-lib,
}:

let
  runtimeLibraries = callPackage ./runtime-libraries.nix { };
in
rustPlatform.buildRustPackage {
  pname = "latch";
  version = (lib.importTOML ../../Cargo.toml).workspace.package.version;

  # The crates include files from scripts/, docs/ and the top level.
  src = lib.fileset.toSource {
    root = ../..;
    fileset = lib.fileset.difference ../.. (
      lib.fileset.unions [
        (lib.fileset.maybeMissing ../../.github)
        (lib.fileset.maybeMissing ../../site)
        ../../packaging
        ../../flake.nix
        ../../flake.lock
      ]
    );
  };

  cargoLock.lockFile = ../../Cargo.lock;
  cargoBuildFlags = [
    "-p"
    "latch-host"
  ];
  # A quick subset of the suite; CI runs all of it on every change.
  cargoTestFlags = [
    "-p"
    "latch-core"
    "-p"
    "latch-proto"
    "-p"
    "latch-codec"
    "--lib"
  ];

  nativeBuildInputs = [
    pkg-config
    addDriverRunpath
    copyDesktopItems
  ];
  buildInputs = [ alsa-lib ];

  desktopItems = [
    (makeDesktopItem {
      name = "latch-host";
      desktopName = "Latch";
      genericName = "Remote Desktop";
      comment = "Stream and control your own machines over Tailscale";
      exec = "latch-host";
      icon = "latch-host";
      categories = [
        "Network"
        "RemoteAccess"
      ];
      keywords = [
        "remote"
        "desktop"
        "streaming"
        "game"
        "tailscale"
      ];
    })
  ];

  postInstall = ''
    install -Dm644 crates/core/assets/logo-1024.png $out/share/pixmaps/latch-host.png
  '';

  # After fixupPhase, which would shrink the rpath back to the linked
  # libraries.
  postFixup = ''
    patchelf --add-rpath ${lib.makeLibraryPath runtimeLibraries} $out/bin/latch-host
    addDriverRunpath $out/bin/latch-host
  '';

  passthru = { inherit runtimeLibraries; };

  meta = {
    description = "Low-latency desktop and game streaming between your own computers over Tailscale";
    homepage = "https://github.com/latch-remote/latch";
    changelog = "https://github.com/latch-remote/latch/blob/main/CHANGELOG.md";
    license = lib.licenses.gpl3Plus;
    mainProgram = "latch-host";
    platforms = lib.platforms.linux;
  };
}
