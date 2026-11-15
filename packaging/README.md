# Packages

Package definitions for Scoop, the AUR and Nix. They install the releases
published on [latch-remote/latch-releases](https://github.com/latch-remote/latch-releases)
and are checked by `.github/workflows/packaging.yml`. Nothing here is
published automatically. User instructions are in the
[README](../README.md#package-managers).

| Path | What it is |
|---|---|
| `scoop/latch.json` | Scoop manifest for `latch-windows-x64.zip` |
| `aur/latch-remote-bin/` | AUR package of the Linux release binaries, x86_64 and aarch64 |
| `aur/latch-remote/` | AUR package built from `latch-X.Y.Z-source.tar.gz` |
| `nix/package.nix` | Nix package built from this tree (`packages.latch` in `flake.nix`) |
| `nix/bin.nix` | Nix package of the Linux release binaries (`packages.latch-bin`) |

The AUR names are `latch-remote` and `latch-remote-bin` because `latch` and
`latch-bin` already belong to an unrelated terminal multiplexer. All
packages install the program as `latch-host`, the release's name for it.

## After a release

```sh
python3 scripts/bump-packaging.py X.Y.Z
```

This reads the release's `SHA256SUMS` and the tagged icon, then rewrites the
Scoop manifest, both PKGBUILDs and their `.SRCINFO`, and `nix/bin.nix`.
Commit the result. CI checks that each `.SRCINFO` matches
`makepkg --printsrcinfo`, builds and installs both AUR packages in an Arch
container, runs `nix flake check` on x86_64 and aarch64, and installs the
Scoop manifest on Windows. `python3 scripts/bump-packaging.py --check`
verifies the committed hashes against the published release. The Nix source
package needs no hashes: it builds the tree with `Cargo.lock`.

Scoop's `checkver` and `autoupdate` let a bucket's own automation follow
releases without this script.

## Publishing

None of these are set up yet. Each needs an account owned by a maintainer.

**Scoop.** Create a public bucket repository (for example
`latch-remote/scoop-bucket`) with `bucket/latch.json` copied from here.
Users run `scoop bucket add latch https://github.com/latch-remote/scoop-bucket`
and `scoop install latch/latch`. The
[bucket template](https://github.com/ScoopInstaller/BucketTemplate) includes
a scheduled Excavator workflow that runs `checkver` and commits new
versions. The Extras bucket is an alternative, subject to its review.

**AUR.** With an AUR account and an SSH key registered on it, for each of
`latch-remote` and `latch-remote-bin`:

```sh
git clone ssh://aur@aur.archlinux.org/latch-remote-bin.git
cp packaging/aur/latch-remote-bin/{PKGBUILD,.SRCINFO,latch-host.desktop} latch-remote-bin/
cd latch-remote-bin && git add -A && git commit -m 'Update to X.Y.Z' && git push
```

The first push creates the package. Later releases repeat the copy after
the bump script.

**Nix.** The flake works directly from this repository
(`nix run github:latch-remote/latch`). For nixpkgs, adapt `nix/package.nix`
to `pkgs/by-name/la/latch/package.nix` with `fetchFromGitHub` on the tag
in place of the local source, and open a pull request against NixOS/nixpkgs
with a maintainer entry.

Once a package is live, link it from the README and from
[issue #81](https://github.com/latch-remote/latch/issues/81).
