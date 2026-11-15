{
  description = "Latch: low-latency desktop and game streaming between your own computers over Tailscale";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      inherit (nixpkgs) lib;
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAllSystems (pkgs: {
        latch = pkgs.callPackage ./packaging/nix/package.nix { };
        latch-bin = pkgs.callPackage ./packaging/nix/bin.nix { };
        default = self.packages.${pkgs.stdenv.hostPlatform.system}.latch;
      });

      overlays.default = final: _prev: {
        latch = final.callPackage ./packaging/nix/package.nix { };
        latch-bin = final.callPackage ./packaging/nix/bin.nix { };
      };

      checks = forAllSystems (
        pkgs:
        let
          packages = self.packages.${pkgs.stdenv.hostPlatform.system};
          runs =
            name: package:
            pkgs.runCommand "${name}-runs" { } ''
              ${lib.getExe package} --version | grep -Fx 'latch-host ${package.version}'
              touch $out
            '';
        in
        {
          inherit (packages) latch latch-bin;
          latch-runs = runs "latch" packages.latch;
          latch-bin-runs = runs "latch-bin" packages.latch-bin;
        }
      );

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [ self.packages.${pkgs.stdenv.hostPlatform.system}.latch ];
          packages = [
            pkgs.clippy
            pkgs.rustfmt
          ];
          LD_LIBRARY_PATH =
            lib.makeLibraryPath
              self.packages.${pkgs.stdenv.hostPlatform.system}.latch.runtimeLibraries;
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
