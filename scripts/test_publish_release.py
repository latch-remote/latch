"""Offline release validation and draft-publication regression checks."""
import copy
import hashlib
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import patch

SPEC = importlib.util.spec_from_file_location("publisher", Path(__file__).with_name("publish-release.py"))
publisher = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(publisher)


class FakeGitHub:
    def __init__(self, draft=None):
        self.release = draft
        self.ref = None
        self.remote = {}
        self.calls = []
        self.corrupt = False
        self.comparison = "identical"
        self.next_id = 1

    def request(self, method, path, body=None, upload=None, **kwargs):
        self.calls.append((method, path, body))
        if upload:
            name = upload.name
            self.remote[name] = {"name": name, "id": self.next_id, "size": upload.stat().st_size, "state": "uploaded", "bytes": upload.read_bytes()}
            self.next_id += 1
            return self.remote[name]
        if method == "DELETE":
            asset_id = int(path.rsplit("/", 1)[1])
            self.remote = {name: asset for name, asset in self.remote.items() if asset["id"] != asset_id}
            return None
        if path == f"repos/{publisher.REPO}":
            return {"full_name": publisher.REPO, "private": False, "default_branch": "main"}
        if "/git/ref/heads/" in path:
            return {"object": {"type": "commit", "sha": "distribution-commit"}}
        if "/git/ref/tags/" in path:
            return self.ref
        if "/compare/" in path:
            return {"status": self.comparison}
        if path.endswith("/git/refs"):
            self.ref = {"object": {"type": "commit", "sha": body["sha"]}}
            return self.ref
        if method == "POST" and path.endswith("/releases"):
            self.release = dict(body, id=17)
            return self.release
        if method == "PATCH":
            self.release.update(body)
            return self.release
        if "/releases/" in path:
            return copy.deepcopy(self.release)
        raise AssertionError((method, path))

    def assets(self, release_id):
        return list(self.remote.values())

    def matches(self, asset, local):
        return not self.corrupt and asset["bytes"] == local.read_bytes()


class ReleaseTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.directory = Path(self.temp.name) / "assets"
        self.directory.mkdir()
        self.version = "6.0.0"
        self.tag = "v6.0.0"
        for name in publisher.expected_names(self.version) - {"release.json", "SHA256SUMS"}:
            (self.directory / name).write_bytes(f"payload {name}".encode())
        for arch in ("x64", "arm64"):
            name = f"latch-host-linux-{arch}.tar.gz"
            digest = publisher.fingerprint(self.directory / name)[1][7:]
            (self.directory / f"{name}.sha256").write_text(f"{digest}  {name}\n")
        self.write_metadata()

    def write_metadata(self):
        payloads = sorted(publisher.expected_names(self.version) - {"release.json", "SHA256SUMS"})
        (self.directory / "SHA256SUMS").write_text("".join(f"{publisher.fingerprint(self.directory / name)[1][7:]}  {name}\n" for name in payloads))
        (self.directory / "release.json").write_text(json.dumps(publisher.manifest_for(self.directory, self.version)))

    def test_manifest_binds_every_payload_and_checksums_without_cycle(self):
        manifest = publisher.validate_plan(self.directory, self.tag)
        self.assertFalse(manifest["prerelease"])
        self.assertEqual(manifest["tag_name"], self.tag)
        self.assertEqual({asset["name"] for asset in manifest["assets"]}, publisher.expected_names(self.version) - {"release.json"})
        for asset in manifest["assets"]:
            data = (self.directory / asset["name"]).read_bytes()
            self.assertEqual(asset["size"], len(data))
            self.assertEqual(asset["digest"], "sha256:" + hashlib.sha256(data).hexdigest())
            self.assertEqual(asset["url"], f"https://github.com/{publisher.REPO}/releases/download/{self.tag}/{asset['name']}")
        sums = (self.directory / "SHA256SUMS").read_text()
        self.assertNotIn("release.json", sums)
        self.assertNotIn("  SHA256SUMS", sums)

    def test_tampered_manifest_fields_and_duplicates_are_rejected(self):
        path = self.directory / "release.json"
        good = json.loads(path.read_text())
        mutations = [
            ("tag_name", "v6.0.1"), ("version", "6.0.1"), ("prerelease", True),
            ("url", "https://api.github.com/private/asset"), ("size", 1),
            ("size", float(good["assets"][0]["size"])), ("prerelease", 0),
            ("digest", "sha256:" + "0" * 64), ("name", "../secret"),
        ]
        for key, value in mutations:
            with self.subTest(key=key):
                bad = copy.deepcopy(good)
                if key in {"tag_name", "version", "prerelease"}:
                    bad[key] = value
                else:
                    bad["assets"][0][key] = value
                path.write_text(json.dumps(bad))
                with self.assertRaises(ValueError):
                    publisher.validate_plan(self.directory, self.tag)
        bad = copy.deepcopy(good)
        bad["assets"].append(bad["assets"][0])
        path.write_text(json.dumps(bad))
        with self.assertRaises(ValueError):
            publisher.validate_plan(self.directory, self.tag)

    def test_missing_extra_directory_symlink_and_size_bounds(self):
        path = self.directory / "latch-host.exe"
        original = path.read_bytes()
        path.unlink()
        with self.assertRaises(ValueError):
            publisher.validate_plan(self.directory, self.tag)
        path.mkdir()
        with self.assertRaises(ValueError):
            publisher.validate_plan(self.directory, self.tag)
        path.rmdir()
        target = Path(self.temp.name) / "private.exe"
        target.write_bytes(original)
        path.symlink_to(target)
        with self.assertRaises(ValueError):
            publisher.validate_plan(self.directory, self.tag)
        path.unlink()
        path.write_bytes(b"")
        with self.assertRaises(ValueError):
            publisher.validate_plan(self.directory, self.tag)
        path.write_bytes(original)
        with patch.object(publisher, "MAX_ASSET", 1):
            with self.assertRaises(ValueError):
                publisher.validate_plan(self.directory, self.tag)
        (self.directory / "private.dump").write_text("private")
        with self.assertRaises(ValueError):
            publisher.validate_plan(self.directory, self.tag)

    def test_versions_and_tag_mismatch_fail_before_api(self):
        for version in ("06.0.0", "v6.0.0", "6.0", "6.0.0/../latest", "6.0.0-01", "6.0.0-rc..1"):
            with self.subTest(version=version), self.assertRaises(ValueError):
                publisher.version_info(version, f"v{version}")
        self.assertTrue(publisher.version_info("6.0.0-rc.1", "v6.0.0-rc.1"))
        api = FakeGitHub()
        with self.assertRaises(ValueError):
            publisher.publish(self.directory, "v6.0.1", "notes", api)
        self.assertEqual(api.calls, [])

    def test_corrupted_payload_and_sidecar_fail(self):
        path = self.directory / "latch-host.exe"
        path.write_bytes(path.read_bytes().replace(b"payload", b"PAYLOAD"))
        with self.assertRaises(ValueError):
            publisher.validate_plan(self.directory, self.tag)
        self.write_metadata()
        sidecar = self.directory / "latch-host-linux-x64.tar.gz.sha256"
        sidecar.write_text("0" * 64 + "  latch-host-linux-x64.tar.gz\n")
        self.write_metadata()
        with self.assertRaises(ValueError):
            publisher.validate_plan(self.directory, self.tag)

    def test_publish_verifies_all_assets_before_single_visibility_change(self):
        api = FakeGitHub()
        publisher.publish(self.directory, self.tag, "Release notes", api)
        self.assertEqual(api.ref["object"]["sha"], "distribution-commit")
        self.assertEqual(set(api.remote), publisher.expected_names(self.version))
        patches = [call for call in api.calls if call[0] == "PATCH"]
        self.assertEqual(len(patches), 1)
        self.assertIs(patches[0][2]["draft"], False)
        self.assertEqual(api.calls[-1], patches[0])
        self.assertIs(api.release["draft"], False)

    def test_existing_draft_is_resumable_but_published_release_is_immutable(self):
        api = FakeGitHub({"draft": True, "tag_name": self.tag, "id": 17})
        api.ref = {"object": {"type": "commit", "sha": "distribution-commit"}}
        for i, path in enumerate(self.directory.iterdir(), 1):
            api.remote[path.name] = {"name": path.name, "id": i, "size": path.stat().st_size, "state": "uploaded", "bytes": path.read_bytes()}
        api.next_id = len(api.remote) + 1
        api.remote["latch-host.exe"]["bytes"] = b"partial upload"
        publisher.publish(self.directory, self.tag, "Release notes", api)
        self.assertEqual(sum(call[0] == "DELETE" for call in api.calls), 1)
        self.assertEqual(sum(call[0] == "POST" for call in api.calls), 1)
        api.calls.clear()
        with self.assertRaisesRegex(ValueError, "immutable"):
            publisher.publish(self.directory, self.tag, "Release notes", api)
        self.assertTrue(all(call[0] == "GET" for call in api.calls))

    def test_remote_corruption_extra_duplicates_or_foreign_tag_keep_draft(self):
        for failure in ("corrupt", "extra", "duplicate", "foreign-tag"):
            api = FakeGitHub({"draft": True, "tag_name": self.tag, "id": 17})
            if failure == "corrupt":
                api.corrupt = True
            if failure == "extra":
                api.remote["private.dump"] = {"name": "private.dump", "id": 100}
            if failure == "duplicate":
                api.assets = lambda _: [{"name": "LICENSE"}, {"name": "LICENSE"}]
            if failure == "foreign-tag":
                api.ref = {"object": {"type": "commit", "sha": "foreign-commit"}}
                api.comparison = "diverged"
            with self.subTest(failure=failure), self.assertRaises(ValueError):
                publisher.publish(self.directory, self.tag, "Release notes", api)
            self.assertIs(api.release["draft"], True)
            self.assertFalse(any(call[0] == "PATCH" for call in api.calls))

    def test_source_scan_preserves_examples_and_rejects_obvious_secrets(self):
        publisher.check_source_file("deploy/relay/.env.example", b"TOKEN=replace-me")
        for name in (".env", "deploy/.env.production", "keys/release.pem", ".aws/credentials", "target/private.dump", "../outside"):
            with self.subTest(name=name), self.assertRaises(ValueError):
                publisher.check_source_file(name, b"data")
        for secret in (b"-----BEGIN " + b"PRIVATE KEY-----", b"ghp_" + b"a" * 36, b"AKIA" + b"X" * 16):
            with self.assertRaises(ValueError):
                publisher.check_source_file("scripts/config.txt", secret)

    def test_source_export_retains_lock_build_inputs_and_licenses_and_excludes_dumps(self):
        files = {
            "Cargo.toml": b'[workspace]\nmembers = ["crates/core"]\n',
            "Cargo.lock": b"exact tagged lockfile\n", "LICENSE": b"GPL-3.0-or-later", "NOTICE": b"third-party licenses",
            "crates/core/Cargo.toml": b"[package]\n", "crates/core/src/lib.rs": b"pub fn example() {}",
            "scripts/bundle-macos.sh": b"bundle", "scripts/install-host.ps1": b"install",
            "scripts/install-macos-release.sh": b"install", ".cargo/config.toml": b"build configuration",
            "docs/measurements/run/private.txt": b"dump", ".github/workflows/release.yml": b"workflow", "site/index.html": b"website",
            ".github/workflows/ci.yml": b"private automation",
            "deploy/relay-tcp/results/private.txt": b"operator record",
            "deploy/relay-tcp/INSPECTION.md": b"operator inventory",
            "deploy/relay-tcp/DEPLOYED.md": b"operator deployment record",
            "deploy/relay-tcp/LIGHTSAIL.md": b"account-specific provisioning record",
            "deploy/relay-tcp/install.sh": b"public install script",
        }
        raw = io.BytesIO()
        with tarfile.open(fileobj=raw, mode="w") as archive:
            for name, data in files.items():
                member = tarfile.TarInfo(name)
                member.size = len(data)
                member.mode = 0o755 if name.endswith(".sh") else 0o644
                archive.addfile(member, io.BytesIO(data))
        def archive_command(command, stdout, **kwargs):
            self.assertEqual(command[-1], "exact-tag-commit")
            stdout.write(raw.getvalue())
        def show(root, *args):
            if args[0] == "ls-tree":
                return b"".join(b"100644 blob " + hashlib.sha1(b"blob " + str(len(data)).encode() + b"\0" + data).hexdigest().encode() + b"\t" + name.encode() + b"\0" for name, data in files.items())
            self.assertEqual(args[0], "show")
            return files[args[1].split(":", 1)[1]]
        destination = Path(self.temp.name) / "source.tar.gz"
        with patch.object(publisher.subprocess, "run", side_effect=archive_command), patch.object(publisher, "git", side_effect=show):
            publisher.source_archive(Path(self.temp.name), "exact-tag-commit", self.version, destination)
        with tarfile.open(destination) as archive:
            names = set(archive.getnames())
            self.assertIn("latch-6.0.0/crates/core/src/lib.rs", names)
            self.assertEqual(archive.extractfile("latch-6.0.0/Cargo.lock").read(), files["Cargo.lock"])
            self.assertEqual(archive.getmember("latch-6.0.0/scripts/bundle-macos.sh").mode, 0o755)
            self.assertFalse(any("measurements" in name or "/site/" in name for name in names))
            self.assertIn(f"latch-{self.version}/.github/workflows/release.yml", names)
            self.assertNotIn(f"latch-{self.version}/.github/workflows/ci.yml", names)
            self.assertIn(f"latch-{self.version}/deploy/relay-tcp/install.sh", names)
            self.assertFalse(any("/results/" in name or name.endswith(("/INSPECTION.md", "/DEPLOYED.md", "/LIGHTSAIL.md")) for name in names))

    def test_changelog_and_checked_out_tag_are_gated(self):
        root = Path(__file__).resolve().parent.parent
        self.assertTrue(publisher.release_notes(root, "5.3.0"))
        with self.assertRaises(ValueError):
            publisher.release_notes(root, "99999.0.0")
        current_version = publisher.tomllib.loads((root / "Cargo.toml").read_text())["workspace"]["package"]["version"]
        with patch.object(publisher, "git", side_effect=[b"checked-out", (root / "Cargo.toml").read_bytes(), b"another-commit"]), patch.object(publisher, "release_notes") as notes:
            with self.assertRaisesRegex(ValueError, "tag does not match"):
                publisher.prepare(root, Path(self.temp.name), Path(self.temp.name) / "output", f"v{current_version}")
            notes.assert_not_called()

    def test_input_artifact_duplicates_and_private_files_fail_before_export(self):
        root = Path(__file__).resolve().parent.parent
        artifacts = Path(self.temp.name) / "artifacts"
        artifacts.mkdir()
        for artifact, names in publisher.ARTIFACTS.items():
            (artifacts / artifact).mkdir()
            for name in names:
                (artifacts / artifact / name).write_bytes(b"test payload")
        for extra in ("latch-windows-x64/latch-macos-arm64.tar.gz", "latch-windows-x64/private.dump"):
            path = artifacts / extra
            path.write_bytes(b"incidental")
            with patch.object(publisher, "git", side_effect=[b"revision", (root / "Cargo.toml").read_bytes()]), patch.object(publisher, "source_archive") as export:
                with self.assertRaisesRegex(ValueError, "allowlist mismatch"):
                    publisher.prepare(root, artifacts, Path(self.temp.name) / "output")
                export.assert_not_called()
            path.unlink()

    def test_export_rejects_symlinks_and_omitted_tracked_source(self):
        destination = Path(self.temp.name) / "source.tar.gz"
        root = Path(self.temp.name)
        link = b"120000 blob " + b"0" * 40 + b"\tcrates/core/src/lib.rs\0"
        with patch.object(publisher, "git", return_value=link):
            with self.assertRaisesRegex(ValueError, "links and submodules"):
                publisher.source_archive(root, "revision", self.version, destination)
        omitted = b"100644 blob " + b"0" * 40 + b"\tcrates/core/src/lib.rs\0"
        def empty_archive(command, stdout, **kwargs):
            with tarfile.open(fileobj=stdout, mode="w"):
                pass
        with patch.object(publisher, "git", return_value=omitted), patch.object(publisher.subprocess, "run", side_effect=empty_archive):
            with self.assertRaisesRegex(ValueError, "missing from export"):
                publisher.source_archive(root, "revision", self.version, destination)

    def test_remote_verification_streams_and_hashes_actual_bytes(self):
        api = publisher.GitHub()
        path = self.directory / "latch-host.exe"
        asset = {"name": path.name, "id": 1, "size": path.stat().st_size, "state": "uploaded"}
        for data in (path.read_bytes(), b"x" * path.stat().st_size):
            def download(method, endpoint, download):
                self.assertTrue(endpoint.endswith("/releases/assets/1"))
                download.write(data)
            with patch.object(api, "request", side_effect=download):
                self.assertEqual(api.matches(asset, path), data == path.read_bytes())

    def test_gh_commands_use_api_auth_upload_and_download(self):
        api = publisher.GitHub()
        def run(command, stdout, **kwargs):
            if "Accept: application/octet-stream" in command:
                stdout.write(b"asset bytes")
            else:
                stdout.write(b'{"id": 1}')
            return subprocess.CompletedProcess(command, 0, stderr=b"")
        with patch.object(publisher.subprocess, "run", side_effect=run) as command:
            self.assertEqual(api.request("POST", "repos/example/releases", {"draft": True}), {"id": 1})
            self.assertEqual(json.loads(command.call_args.kwargs["input"]), {"draft": True})
            path = self.directory / "LICENSE"
            api.request("POST", "https://uploads.github.com/example", upload=path)
            self.assertIn(str(path), command.call_args.args[0])
            download = io.BytesIO()
            api.request("GET", "repos/example/releases/assets/1", download=download)
            self.assertEqual(download.getvalue(), b"asset bytes")
        result = subprocess.CompletedProcess([], 1, stderr=b"gh: Not Found (HTTP 404)")
        with patch.object(publisher.subprocess, "run", return_value=result):
            self.assertIsNone(api.request("GET", "repos/example/missing", optional=True))
            with self.assertRaises(RuntimeError):
                api.request("GET", "repos/example/missing")


if __name__ == "__main__":
    unittest.main()
