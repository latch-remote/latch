# The published Linux release binary, patched to run on Nix.
# scripts/bump-packaging.py updates the version and hashes.
{
  lib,
  stdenv,
  callPackage,
  fetchurl,
  autoPatchelfHook,
  addDriverRunpath,
  copyDesktopItems,
  alsa-lib,
}:

let
  version = "6.1.1";
  assets = {
    x86_64-linux = {
      arch = "x64";
      hash = "sha256-DHbn01OgbedQ9fQSB7S5oJ4XIStlIK37IkvNX0KBbzc=";
    };
    aarch64-linux = {
      arch = "arm64";
      hash = "sha256-Yoil3yFt8a6HVdtN9qhZYrhW7452chV2mCxgm76y2q0=";
    };
  };
  asset =
    assets.${stdenv.hostPlatform.system}
      or (throw "latch-bin: no release binary for ${stdenv.hostPlatform.system}");
  latch = callPackage ./package.nix { };
in
stdenv.mkDerivation {
  pname = "latch-bin";
  inherit version;

  src = fetchurl {
    url = "https://github.com/latch-remote/latch-releases/releases/download/v${version}/latch-host-linux-${asset.arch}.tar.gz";
    inherit (asset) hash;
  };

  nativeBuildInputs = [
    autoPatchelfHook
    copyDesktopItems
  ];
  buildInputs = [
    alsa-lib
    (lib.getLib stdenv.cc.cc)
  ];
  # The library outputs: libva's default output is `dev`, which has no .so.
  runtimeDependencies = map lib.getLib latch.runtimeLibraries;
  # autoPatchelf rewrites RUNPATH in postFixup, so the driver directory
  # (NVENC, CUDA) goes in through it rather than after it.
  appendRunpaths = [ "${addDriverRunpath.driverLink}/lib" ];
  inherit (latch) desktopItems;

  dontConfigure = true;
  dontBuild = true;
  dontStrip = true;

  installPhase = ''
    runHook preInstall
    install -Dm755 latch-host $out/bin/latch-host
    install -Dm644 NOTICE $out/share/licenses/latch-bin/NOTICE
    install -Dm644 ${../../crates/core/assets/logo-1024.png} $out/share/pixmaps/latch-host.png
    runHook postInstall
  '';

  meta = latch.meta // {
    description = "${latch.meta.description} (release binary)";
    sourceProvenance = [ lib.sourceTypes.binaryNativeCode ];
    platforms = builtins.attrNames assets;
  };
}
