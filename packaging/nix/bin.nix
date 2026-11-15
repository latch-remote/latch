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
  version = "6.0.0";
  assets = {
    x86_64-linux = {
      arch = "x64";
      hash = "sha256-HdQRZgVp/BlnGU30Jn0KLXvmohGK6O5hldK27z5qfD0=";
    };
    aarch64-linux = {
      arch = "arm64";
      hash = "sha256-OuwQPeTDYcrsiWEF+J9CrzOF3HXETXgdfE4ypfP1xeQ=";
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
    addDriverRunpath
    copyDesktopItems
  ];
  buildInputs = [
    alsa-lib
    (lib.getLib stdenv.cc.cc)
  ];
  runtimeDependencies = latch.runtimeLibraries;
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

  postFixup = ''
    addDriverRunpath $out/bin/latch-host
  '';

  meta = latch.meta // {
    description = "${latch.meta.description} (release binary)";
    sourceProvenance = [ lib.sourceTypes.binaryNativeCode ];
    platforms = builtins.attrNames assets;
  };
}
