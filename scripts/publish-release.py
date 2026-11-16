#!/usr/bin/env python3
"""Prepare an allowlisted release, then publish a verified public draft.

Preparation is offline. Only the publish commands use GH_TOKEN and GitHub's API.
The distribution tag always refers to its own default-branch history.

`publish-bridge` also puts the archives that Latch 4.x and 5.x updaters look
for on the source repository's own release for the same tag. Those versions
ask the source repository (through GitHub's rename redirects) for its latest
release, so they find a new version only there, and only while it is public.
"""
import argparse
import gzip
import hashlib
import importlib.util
import io
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import subprocess
import tarfile
import tempfile
import tomllib
from urllib.parse import quote

REPO = "latch-remote/latch-releases"
SOURCE_REPO = "latch-remote/latch"
MAX_ASSET = 512 * 1024 * 1024  # Same bound as the core updater.
ARTIFACTS = {
    "latch-macos-arm64": ("latch-macos-arm64.tar.gz", "brolink-macos-arm64.tar.gz"),
    "latch-windows-x64": (
        "latch-windows-x64.zip", "brolink-windows-x64.zip", "latch-host.exe"
    ),
    "latch-host-linux-x64": (
        "latch-host-linux-x64.tar.gz", "latch-host-linux-x64.tar.gz.sha256"
    ),
    "latch-host-linux-arm64": (
        "latch-host-linux-arm64.tar.gz", "latch-host-linux-arm64.tar.gz.sha256"
    ),
}
# Built only for 4.0 updaters, which know nothing of the distribution
# repository: they go to the bridge release, never to latch-releases.
LEGACY = ("brolink-macos-arm64.tar.gz", "brolink-windows-x64.zip")
# What 4.1 to 5.x updaters download and verify by GitHub's asset digest.
BRIDGED = ("latch-macos-arm64.tar.gz", "latch-windows-x64.zip", "latch-host.exe")
BRIDGE_NAMES = frozenset(LEGACY + BRIDGED + ("SHA256SUMS",))
VERSION = re.compile(
    r"(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
    r"(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?"
    r"(?:\+[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"
)
# These public test credentials are required to build and run relay tests.
TEST_CERTS = {
    "crates/relay/tests/certs/ca.pem": "3ba85c89957262653b4c32e44f011eaa16d08553b5ad270d6512dffd0a88dd65",
    "crates/relay/tests/certs/server.pem": "f337c365b6fef37b9f2c8465bc0f7f93043a9a5bdad09e1bb681127f970e9ee4",
    "crates/relay/tests/certs/server-key.pem": "63fefef4719b362c2a501a1a9baf734c8472db7fac3903ed16eca1db4b6104d8",
}
SECRET = re.compile(
    rb"-----BEGIN (?:[A-Z0-9]+ )*PRIVATE KEY-----|"
    rb"(?:gh[pousr]_[A-Za-z0-9]{30,}|github_pat_[A-Za-z0-9_]{40,}|AKIA[A-Z0-9]{16})"
)


def require(condition, message):
    if not condition:
        raise ValueError(message)


def version_info(version, tag):
    match = VERSION.fullmatch(version)
    require(match is not None and tag == f"v{version}", "invalid version or mismatched tag")
    pre = match.group(4)
    if pre:
        require(
            all(not part.isdigit() or part == "0" or not part.startswith("0")
                for part in pre.split(".")),
            "noncanonical prerelease version",
        )
    return bool(pre)


def expected_names(version):
    return {name for names in ARTIFACTS.values() for name in names if name not in LEGACY} | {
        f"latch-{version.replace('+', '-')}-source.tar.gz", "LICENSE", "NOTICE", "SHA256SUMS", "release.json"
    }


def fingerprint(path):
    require(not path.is_symlink() and path.is_file(), f"asset must be a regular file: {path.name}")
    size = path.stat().st_size
    require(0 < size <= MAX_ASSET, f"asset size out of bounds: {path.name}")
    with path.open("rb") as stream:
        digest = hashlib.file_digest(stream, "sha256").hexdigest()
    return size, f"sha256:{digest}"


def git(root, *args):
    return subprocess.check_output(["git", "-C", str(root), *args])


def release_notes(root, version, required=True):
    spec = importlib.util.spec_from_file_location("release_notes", root / "scripts/release-notes.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    text = (root / "CHANGELOG.md").read_text()
    headings = [line.split()[1:2] for line in text.splitlines() if line.startswith("## ")]
    require(
        not required or headings.count([version]) == 1,
        "release requires exactly one matching changelog section",
    )
    notes = "\n".join(module.unwrap(module.section(text, version))).strip()
    require(not required or bool(notes), "release changelog section is empty")
    return notes


def check_source_file(name, data):
    path = PurePosixPath(name)
    require(not path.is_absolute() and ".." not in path.parts and "\\" not in name, "unsafe source path")
    forbidden = {".git", ".aws", ".ssh", "target", "__pycache__", "node_modules", "dist", "dist.noindex"}
    require(not forbidden.intersection(path.parts), f"incidental source path: {name}")
    fixture = name in TEST_CERTS and hashlib.sha256(data).hexdigest() == TEST_CERTS[name]
    lower = path.name.lower()
    example = lower in {".env.example", "env.example"}
    secret_name = (
        lower == ".env" or lower.startswith(".env.")
        or lower in {"credentials", "id_rsa", "id_ed25519"}
        or lower.endswith((".pem", ".key", ".p12", ".pfx", ".keystore"))
    )
    require(not secret_name or example or fixture, f"secret-like source filename: {name}")
    require(fixture or SECRET.search(data) is None, f"secret-like source contents: {name}")


def excluded_source(name):
    # Packaging commands live in this workflow and belong with build scripts.
    if name in {".github", ".github/workflows", ".github/workflows/release.yml"}:
        return False
    return (name.split("/")[0] in {".github", "site"}
            or name == "docs/measurements" or name.startswith("docs/measurements/")
            or name == "deploy/relay-tcp/results" or name.startswith("deploy/relay-tcp/results/")
            or name in {
                "deploy/relay-tcp/INSPECTION.md", "deploy/relay-tcp/DEPLOYED.md",
                "deploy/relay-tcp/LIGHTSAIL.md",
            })


def source_archive(root, revision, version, destination):
    """Export tracked tag bytes, retaining build inputs and third-party notices."""
    prefix = f"latch-{version}/"
    names = set()
    tree = {}
    for entry in git(root, "ls-tree", "-r", "-z", revision).split(b"\0"):
        if not entry:
            continue
        metadata, path = entry.split(b"\t", 1)
        name = path.decode()
        if excluded_source(name):
            continue
        mode, kind, digest = metadata.decode().split()
        require(kind == "blob" and mode in {"100644", "100755"}, f"source links and submodules forbidden: {name}")
        tree[name] = digest
    with tempfile.TemporaryFile() as raw:
        subprocess.run(["git", "-C", str(root), "archive", "--format=tar", revision], stdout=raw, check=True)
        raw.seek(0)
        with tarfile.open(fileobj=raw) as source, destination.open("wb") as output:
            # Stable gzip metadata makes draft reruns reproducible.
            with (
                gzip.GzipFile(filename="", mode="wb", fileobj=output, mtime=0) as zipped,
                tarfile.open(fileobj=zipped, mode="w|") as archive,
            ):
                for member in source:
                    name = member.name.rstrip("/")
                    if excluded_source(name):
                        continue
                    require(member.isdir() or member.isfile(), f"source links and special files forbidden: {name}")
                    require(name not in names, f"duplicate source entry: {name}")
                    names.add(name)
                    if member.isfile():
                        data = source.extractfile(member).read()
                        check_source_file(name, data)
                        require(name in tree, f"untracked source archive entry: {name}")
                        blob = b"blob " + str(len(data)).encode() + b"\0" + data
                        algorithm = "sha1" if len(tree[name]) == 40 else "sha256"
                        require(hashlib.new(algorithm, blob).hexdigest() == tree[name], f"source bytes differ from tag: {name}")
                    member.name = prefix + name
                    member.pax_headers = {}
                    if member.isfile():
                        archive.addfile(member, io.BytesIO(data))
                    else:
                        archive.addfile(member)
    require(set(tree) <= names, f"tracked source files missing from export: {sorted(set(tree) - names)}")
    required = {
        "Cargo.toml", "Cargo.lock", "LICENSE", "NOTICE", "scripts/bundle-macos.sh",
        "scripts/install-host.ps1", "scripts/install-macos-release.sh",
        ".github/workflows/release.yml",
    }
    workspace = tomllib.loads(git(root, "show", f"{revision}:Cargo.toml").decode())["workspace"]
    required.update(f"{member}/Cargo.toml" for member in workspace["members"])
    require(required <= names, f"source export missing build inputs: {sorted(required - names)}")
    with tarfile.open(destination) as archive:
        require({m.name.removeprefix(prefix) for m in archive} == names, "source export structure mismatch")
        for name in ("Cargo.lock", "Cargo.toml", "LICENSE", "NOTICE"):
            require(archive.extractfile(prefix + name).read() == git(root, "show", f"{revision}:{name}"), f"source export differs from tag: {name}")


def manifest_for(directory, version):
    prerelease = version_info(version, f"v{version}")
    assets = []
    for path in sorted(directory.iterdir()):
        if path.name == "release.json":
            continue  # A manifest cannot hash itself.
        size, digest = fingerprint(path)
        assets.append({
            "name": path.name,
            "url": f"https://github.com/{REPO}/releases/download/v{version}/{path.name}",
            "size": size,
            "digest": digest,
        })
    return {"version": version, "tag_name": f"v{version}", "prerelease": prerelease, "assets": assets}


def validate_plan(directory, tag):
    require(not directory.is_symlink() and directory.is_dir(), "release directory must be a real directory")
    manifest = json.loads((directory / "release.json").read_text())
    require(type(manifest) is dict, "manifest must be an object")
    version = manifest["version"]
    require(type(version) is str and type(manifest.get("prerelease")) is bool, "invalid manifest field types")
    require(type(manifest.get("assets")) is list, "manifest assets must be an array")
    for asset in manifest["assets"]:
        require(type(asset) is dict and type(asset.get("size")) is int, "asset size must be an integer")
    version_info(version, tag)
    require({path.name for path in directory.iterdir()} == expected_names(version), "release asset allowlist mismatch")
    for path in directory.iterdir():
        fingerprint(path)
    require(manifest == manifest_for(directory, version), "manifest does not match release bytes, URLs, version, or prerelease status")
    # SHA256SUMS covers payloads. The manifest hashes SHA256SUMS and payloads.
    # Neither SHA256SUMS nor release.json hashes itself or forms a cycle.
    payloads = sorted(expected_names(version) - {"release.json", "SHA256SUMS"})
    sums = "".join(f"{fingerprint(directory / name)[1][7:]}  {name}\n" for name in payloads)
    require((directory / "SHA256SUMS").read_text() == sums, "SHA256SUMS does not match payloads")
    for arch in ("x64", "arm64"):
        name = f"latch-host-linux-{arch}.tar.gz"
        require((directory / f"{name}.sha256").read_text() == f"{fingerprint(directory / name)[1][7:]}  {name}\n", f"invalid Linux checksum: {arch}")
    return manifest


def sums_for(directory, names):
    return "".join(f"{fingerprint(directory / name)[1][7:]}  {name}\n" for name in sorted(names))


def validate_bridge(bridge, release, tag):
    """The bridge holds the legacy archives and byte-identical copies of the
    release's updater archives, with a SHA256SUMS covering exactly those."""
    manifest = validate_plan(release, tag)
    require(not bridge.is_symlink() and bridge.is_dir(), "bridge directory must be a real directory")
    require({path.name for path in bridge.iterdir()} == BRIDGE_NAMES, "bridge asset allowlist mismatch")
    for path in bridge.iterdir():
        fingerprint(path)
    for name in BRIDGED:
        require(fingerprint(bridge / name) == fingerprint(release / name), f"bridge copy differs from release: {name}")
    require((bridge / "SHA256SUMS").read_text() == sums_for(bridge, BRIDGE_NAMES - {"SHA256SUMS"}), "bridge SHA256SUMS does not match payloads")
    return manifest


def prepare(root, artifacts, output, tag=None, bridge=None):
    require(not root.is_symlink() and not artifacts.is_symlink(), "input directories must be real directories")
    version = tomllib.loads((root / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    version_info(version, tag or f"v{version}")
    revision = git(root, "rev-parse", "HEAD").decode().strip()
    require((root / "Cargo.toml").read_bytes() == git(root, "show", f"{revision}:Cargo.toml"), "workspace metadata differs from checked-out revision")
    if tag:
        require(git(root, "rev-parse", f"refs/tags/{tag}^{{commit}}").decode().strip() == revision, "tag does not match checked-out source")
    notes = release_notes(root, version, required=tag is not None)
    expected = {f"{artifact}/{name}" for artifact, names in ARTIFACTS.items() for name in names}
    found = set()
    for path in artifacts.rglob("*"):
        require(not path.is_symlink(), "artifact symlinks forbidden")
        if not path.is_dir():
            found.add(path.relative_to(artifacts).as_posix())
        else:
            require(path.relative_to(artifacts).as_posix() in ARTIFACTS, "unexpected artifact directory")
    require(found == expected, f"input artifact allowlist mismatch, missing={sorted(expected - found)}, extra={sorted(found - expected)}")
    output.mkdir()  # Refuse reuse of a partial or contaminated output directory.
    if bridge is not None:
        bridge.mkdir()
    for name in sorted(expected):
        path = artifacts / name
        fingerprint(path)
        if path.name not in LEGACY:
            shutil.copyfile(path, output / path.name)
        if bridge is not None and path.name in LEGACY + BRIDGED:
            shutil.copyfile(path, bridge / path.name)
    for name in ("LICENSE", "NOTICE"):
        (output / name).write_bytes(git(root, "show", f"{revision}:{name}"))
    source_archive(root, revision, version, output / f"latch-{version.replace('+', '-')}-source.tar.gz")
    payloads = sorted(output.iterdir())
    (output / "SHA256SUMS").write_text("".join(f"{fingerprint(path)[1][7:]}  {path.name}\n" for path in payloads))
    (output / "release.json").write_text(json.dumps(manifest_for(output, version), indent=2) + "\n")
    validate_plan(output, tag or f"v{version}")
    if bridge is not None:
        (bridge / "SHA256SUMS").write_text(sums_for(bridge, BRIDGE_NAMES - {"SHA256SUMS"}))
        validate_bridge(bridge, output, tag or f"v{version}")
    return notes


class GitHub:
    """Use the runner's gh client for authenticated API calls and asset streams."""
    def __init__(self, repo=REPO):
        self.repo = repo

    def request(self, method, path, body=None, upload=None, download=None, optional=False):
        command = ["gh", "api", "--method", method, path, "-H", "X-GitHub-Api-Version: 2022-11-28"]
        data = None
        if download is not None:
            command += ["-H", "Accept: application/octet-stream"]
        elif upload:
            command += ["-H", "Content-Type: application/octet-stream", "--input", str(upload)]
        elif body is not None:
            command += ["--input", "-"]
            data = json.dumps(body).encode()
        with tempfile.TemporaryFile() as stream:
            result = subprocess.run(command, input=data, stdout=stream, stderr=subprocess.PIPE)
            if result.returncode:
                if optional and b"(HTTP 404)" in result.stderr:
                    return None
                raise RuntimeError(f"GitHub API failed: {method} {path.split('?')[0]} (exit {result.returncode})")
            stream.seek(0)
            if download is not None:
                shutil.copyfileobj(stream, download)
                return None
            content = stream.read()
            return json.loads(content) if content.strip() else None

    def assets(self, release_id):
        assets = []
        page = 1
        while True:
            batch = self.request("GET", f"repos/{self.repo}/releases/{release_id}/assets?per_page=100&page={page}")
            assets.extend(batch)
            if len(batch) < 100:
                return assets
            page += 1

    def matches(self, asset, local):
        if asset.get("size") != local.stat().st_size or asset.get("state") != "uploaded":
            return False
        with tempfile.TemporaryFile() as download:
            self.request("GET", f"repos/{self.repo}/releases/assets/{asset['id']}", download=download)
            require(download.tell() <= MAX_ASSET, "remote asset exceeds size bound")
            size = download.tell()
            download.seek(0)
            digest = hashlib.file_digest(download, "sha256").hexdigest()
        return (size, f"sha256:{digest}") == fingerprint(local)


def asset_map(assets, allowed):
    names = [asset["name"] for asset in assets]
    require(len(names) == len(set(names)), "duplicate remote assets")
    require(set(names) <= allowed, "unexpected remote assets in draft")
    return {asset["name"]: asset for asset in assets}


def sync_assets(api, repo, release_id, directory, allowed):
    """Upload what is missing or different, then verify every remote byte."""
    remote = asset_map(api.assets(release_id), allowed)
    for name in sorted(allowed):
        path = directory / name
        if name in remote and api.matches(remote[name], path):
            continue
        if name in remote:
            api.request("DELETE", f"repos/{repo}/releases/assets/{remote[name]['id']}")
        api.request(
            "POST",
            f"https://uploads.github.com/repos/{repo}/releases/{release_id}/assets?name={quote(name, safe='')}",
            upload=path,
        )
    remote = asset_map(api.assets(release_id), allowed)
    require(set(remote) == allowed, "remote release asset list is incomplete")
    for name, asset in remote.items():
        require(api.matches(asset, directory / name), f"remote checksum verification failed: {name}")


def publish(directory, tag, notes, api=None):
    manifest = validate_plan(directory, tag)
    require(bool(notes.strip()), "publication requires nonempty release notes")
    api = api or GitHub()
    repository = api.request("GET", f"repos/{REPO}")
    require(repository["full_name"] == REPO and repository["private"] is False, "distribution repository must be the expected public repository")
    release = api.request("GET", f"repos/{REPO}/releases/tags/{tag}", optional=True)
    require(release is None or release["draft"] is True, "published versions are immutable, refusing overwrite")
    branch = repository["default_branch"]
    base = api.request("GET", f"repos/{REPO}/git/ref/heads/{quote(branch, safe='')}")["object"]
    require(base["type"] == "commit", "distribution branch must point to a commit")
    ref = api.request("GET", f"repos/{REPO}/git/ref/tags/{tag}", optional=True)
    if ref:
        require(ref["object"]["type"] == "commit", "distribution tag must directly reference a distribution commit")
        comparison = api.request("GET", f"repos/{REPO}/compare/{base['sha']}...{ref['object']['sha']}")
        require(comparison["status"] in {"identical", "behind"}, "tag must belong to distribution default-branch history")
    else:
        ref = api.request("POST", f"repos/{REPO}/git/refs", {"ref": f"refs/tags/{tag}", "sha": base["sha"]})
    target = ref["object"]["sha"]
    details = {
        "name": f"Latch {tag}",
        "body": notes,
        "prerelease": manifest["prerelease"],
    }
    if release is None:
        release = api.request("POST", f"repos/{REPO}/releases", {
            **details, "tag_name": tag, "target_commitish": target, "draft": True,
        })
    require(release["tag_name"] == tag and release["draft"] is True, "release must be the requested draft")
    release_id = release["id"]
    sync_assets(api, REPO, release_id, directory, expected_names(manifest["version"]))
    require(validate_plan(directory, tag) == manifest, "local release plan changed during publication")
    # Recheck both state and tag immediately before making the draft visible.
    current = api.request("GET", f"repos/{REPO}/releases/{release_id}")
    require(current["draft"] is True and current["tag_name"] == tag, "draft changed during publication")
    current_ref = api.request("GET", f"repos/{REPO}/git/ref/tags/{tag}")
    require(current_ref["object"] == ref["object"], "distribution tag changed during publication")
    published = api.request("PATCH", f"repos/{REPO}/releases/{release_id}", {
        **details,
        "draft": False,
        "make_latest": "false" if manifest["prerelease"] else "true",
    })
    require(published["draft"] is False and published["tag_name"] == tag, "publication response does not match plan")
    print(f"Published https://github.com/{REPO}/releases/tag/{tag}")


def bridge_notes(notes, tag):
    return (
        f"{notes.strip()}\n\n---\n\n"
        f"Download Latch from the [full release on latch-releases](https://github.com/{REPO}/releases/tag/{tag}), "
        "which also has the Linux builds, the source and `SHA256SUMS` for every file. "
        "This copy of the macOS and Windows archives is here so that Latch 4.x and 5.x, "
        "which check this repository for updates, can update themselves.\n"
    )


def publish_bridge(bridge, release, tag, notes, repo=SOURCE_REPO, api=None):
    manifest = validate_bridge(bridge, release, tag)
    require(bool(notes.strip()), "publication requires nonempty release notes")
    require(repo != REPO, "the bridge belongs on the source repository, not the distribution repository")
    api = api or GitHub(repo)
    repository = api.request("GET", f"repos/{repo}")
    require(repository["full_name"] == repo and repository["private"] is False, "bridge repository must be the expected public repository")
    # The tag push that runs this workflow created the tag; never move it.
    ref = api.request("GET", f"repos/{repo}/git/ref/tags/{tag}")
    require(ref["object"]["type"] in {"commit", "tag"}, "source tag must reference a commit or an annotated tag")
    release_info = api.request("GET", f"repos/{repo}/releases/tags/{tag}", optional=True)
    details = {"name": f"Latch {tag}", "body": bridge_notes(notes, tag), "prerelease": manifest["prerelease"]}
    if release_info is not None and release_info["draft"] is False:
        # A rerun after success: accept it only if it is exactly this bridge.
        remote = asset_map(api.assets(release_info["id"]), BRIDGE_NAMES)
        require(set(remote) == BRIDGE_NAMES and all(api.matches(asset, bridge / name) for name, asset in remote.items()),
                "published bridge differs from this release, refusing overwrite")
        print(f"Bridge already published at https://github.com/{repo}/releases/tag/{tag}")
        return
    if release_info is None:
        release_info = api.request("POST", f"repos/{repo}/releases", {**details, "tag_name": tag, "draft": True})
    require(release_info["tag_name"] == tag and release_info["draft"] is True, "bridge release must be the requested draft")
    release_id = release_info["id"]
    sync_assets(api, repo, release_id, bridge, BRIDGE_NAMES)
    require(validate_bridge(bridge, release, tag) == manifest, "local bridge plan changed during publication")
    current = api.request("GET", f"repos/{repo}/releases/{release_id}")
    require(current["draft"] is True and current["tag_name"] == tag, "bridge draft changed during publication")
    require(api.request("GET", f"repos/{repo}/git/ref/tags/{tag}")["object"] == ref["object"], "source tag changed during publication")
    published = api.request("PATCH", f"repos/{repo}/releases/{release_id}", {
        **details,
        "draft": False,
        "make_latest": "false" if manifest["prerelease"] else "true",
    })
    require(published["draft"] is False and published["tag_name"] == tag, "bridge publication response does not match plan")
    print(f"Published bridge https://github.com/{repo}/releases/tag/{tag}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    subcommands = parser.add_subparsers(dest="command", required=True)
    build = subcommands.add_parser("prepare")
    build.add_argument("--root", type=Path, default=Path.cwd())
    build.add_argument("--artifacts", type=Path, required=True)
    build.add_argument("--output", type=Path, required=True)
    build.add_argument("--tag")
    build.add_argument("--notes", type=Path, required=True)
    build.add_argument("--bridge-output", type=Path)
    send = subcommands.add_parser("publish")
    send.add_argument("--assets", type=Path, required=True)
    send.add_argument("--tag", required=True)
    send.add_argument("--notes", type=Path, required=True)
    legacy = subcommands.add_parser("publish-bridge")
    legacy.add_argument("--assets", type=Path, required=True)
    legacy.add_argument("--release-assets", type=Path, required=True)
    legacy.add_argument("--tag", required=True)
    legacy.add_argument("--notes", type=Path, required=True)
    legacy.add_argument("--repo", default=SOURCE_REPO)
    args = parser.parse_args()
    try:
        if args.command == "prepare":
            notes = prepare(args.root.resolve(), args.artifacts, args.output, args.tag, args.bridge_output)
            args.notes.write_text(notes + "\n")
        elif args.command == "publish":
            require(bool(os.environ.get("GH_TOKEN")), "GH_TOKEN is required for publication")
            publish(args.assets, args.tag, args.notes.read_text())
        else:
            require(bool(os.environ.get("GH_TOKEN")), "GH_TOKEN is required for publication")
            publish_bridge(args.assets, args.release_assets, args.tag, args.notes.read_text(), args.repo)
    except (ValueError, KeyError, OSError, RuntimeError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"release validation failed: {error}\n")


if __name__ == "__main__":
    main()
