#!/usr/bin/env python3
"""Point the packaging manifests at a published release.

    scripts/bump-packaging.py 6.1.0     rewrite them for v6.1.0
    scripts/bump-packaging.py --check   fail unless they match their release

The hashes come from the release's SHA256SUMS on latch-remote/latch-releases,
the icon from the tag in latch-remote/latch, and the desktop entry from this
tree. Scoop's manifest, both PKGBUILDs with their .SRCINFO, and the Nix
release-binary package are updated together; nothing is published.
"""
import argparse
import base64
import hashlib
import json
from pathlib import Path
import re
import sys
import urllib.request

ROOT = Path(__file__).resolve().parent.parent
PACKAGING = ROOT / "packaging"
SCOOP = PACKAGING / "scoop" / "latch.json"
AUR = [PACKAGING / "aur" / "latch-remote", PACKAGING / "aur" / "latch-remote-bin"]
NIX_BIN = PACKAGING / "nix" / "bin.nix"
RELEASES = "https://github.com/latch-remote/latch-releases/releases/download"
ICON = "https://raw.githubusercontent.com/latch-remote/latch/v{version}/crates/core/assets/logo-1024.png"
STABLE = re.compile(r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)")
NIX_ARCH = {"x64": "latch-host-linux-x64.tar.gz", "arm64": "latch-host-linux-arm64.tar.gz"}


def fetch(url):
    with urllib.request.urlopen(url, timeout=60) as response:
        return response.read()


def sha256(data):
    return hashlib.sha256(data).hexdigest()


def release_sums(version, sums_file=None):
    text = Path(sums_file).read_text() if sums_file else fetch(f"{RELEASES}/v{version}/SHA256SUMS").decode()
    sums = {}
    for line in text.splitlines():
        if not line.strip():
            continue
        digest, name = line.split(maxsplit=1)
        if not re.fullmatch(r"[0-9a-f]{64}", digest):
            raise ValueError(f"malformed SHA256SUMS line: {line!r}")
        sums[name.lstrip("*")] = digest
    return sums


def asset_name(url):
    """The release asset or local file a source entry refers to."""
    url = url.split("::", 1)[-1]
    return url.rsplit("/", 1)[-1]


def srcinfo_pairs(text):
    """(source, sha256) for every source in a .SRCINFO, per architecture."""
    sources, sums = {}, {}
    for line in text.splitlines():
        key, _, value = line.strip().partition(" = ")
        if key.startswith("source"):
            sources.setdefault(key[len("source"):], []).append(value)
        elif key.startswith("sha256sums"):
            sums.setdefault(key[len("sha256sums"):], []).append(value)
    if sources.keys() != sums.keys() or any(len(sources[k]) != len(sums[k]) for k in sources):
        raise ValueError("every source needs exactly one sha256sum")
    return [pair for k in sources for pair in zip(sources[k], sums[k])]


def current_version():
    return json.loads(SCOOP.read_text())["version"]


def plan(version, sums, icon_digest):
    """Every file's new contents, keyed by path."""
    old = current_version()
    out = {}

    def digest_for(name, package_dir):
        if name == "logo-1024.png":
            return icon_digest
        local = package_dir / name
        if local.is_file():
            return sha256(local.read_bytes())
        release_name = name.replace(old, version)
        if release_name not in sums:
            raise ValueError(f"{release_name} is not in the release's SHA256SUMS")
        return sums[release_name]

    manifest = json.loads(SCOOP.read_text())
    zip_name = "latch-windows-x64.zip"
    manifest["version"] = version
    arch = manifest["architecture"]["64bit"]
    arch["url"] = f"{RELEASES}/v{version}/{zip_name}"
    arch["hash"] = sums[zip_name]
    out[SCOOP] = json.dumps(manifest, indent=4, ensure_ascii=False) + "\n"

    for package_dir in AUR:
        pkgbuild = (package_dir / "PKGBUILD").read_text()
        srcinfo = (package_dir / ".SRCINFO").read_text()
        replacements = {}
        for source, digest in srcinfo_pairs(srcinfo):
            replacements[digest] = digest_for(asset_name(source), package_dir)
        for old_digest, new_digest in replacements.items():
            pkgbuild = pkgbuild.replace(old_digest, new_digest)
            srcinfo = srcinfo.replace(old_digest, new_digest)
        pkgbuild = re.sub(r"(?m)^pkgver=.*$", f"pkgver={version}", pkgbuild)
        srcinfo = re.sub(r"(?m)^\tpkgver = .*$", f"\tpkgver = {version}", srcinfo)
        if version != old:
            pkgbuild = re.sub(r"(?m)^pkgrel=.*$", "pkgrel=1", pkgbuild)
            srcinfo = re.sub(r"(?m)^\tpkgrel = .*$", "\tpkgrel = 1", srcinfo)
            srcinfo = "\n".join(
                line.replace(old, version) if line.lstrip().startswith("source") else line
                for line in srcinfo.split("\n")
            )
        out[package_dir / "PKGBUILD"] = pkgbuild
        out[package_dir / ".SRCINFO"] = srcinfo

    nix = NIX_BIN.read_text()
    nix = re.sub(r'(?m)^(  version = )"[^"]*";', rf'\g<1>"{version}";', nix)
    for arch_name, asset in NIX_ARCH.items():
        sri = "sha256-" + base64.b64encode(bytes.fromhex(sums[asset])).decode()
        nix, count = re.subn(
            rf'(arch = "{arch_name}";\s*hash = )"[^"]*";', rf'\g<1>"{sri}";', nix
        )
        if count != 1:
            raise ValueError(f"no {arch_name} hash in {NIX_BIN}")
    out[NIX_BIN] = nix
    return out


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("version", nargs="?", help="release version, without the v")
    parser.add_argument("--check", action="store_true", help="change nothing; fail if a file is out of date")
    parser.add_argument("--sums", help="a local SHA256SUMS instead of the release's")
    parser.add_argument("--icon", help="a local logo-1024.png instead of the tag's")
    args = parser.parse_args()
    version = args.version or current_version()
    if not STABLE.fullmatch(version):
        parser.error("packages follow stable releases only (X.Y.Z)")
    if not args.check and not args.version:
        parser.error("give a version, or --check")

    sums = release_sums(version, args.sums)
    icon = Path(args.icon).read_bytes() if args.icon else fetch(ICON.format(version=version))
    stale = []
    for path, text in plan(version, sums, sha256(icon)).items():
        if path.read_text() != text:
            stale.append(path.relative_to(ROOT))
            if not args.check:
                path.write_text(text)
    if args.check:
        if stale:
            print(f"out of date for {version}: " + ", ".join(map(str, stale)), file=sys.stderr)
            return 1
        print(f"packaging matches the published {version}")
    else:
        for path in stale:
            print(f"updated {path}")
        print("check the .SRCINFO files with `makepkg --printsrcinfo` (CI does)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
