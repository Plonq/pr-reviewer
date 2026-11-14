#!/usr/bin/env python3
"""Release automation checks. Uses disposable repositories, never GitHub writes."""
import contextlib
import importlib.util
import io
import os
from pathlib import Path
import plistlib
import shutil
import subprocess
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parent.parent
spec = importlib.util.spec_from_file_location("release", ROOT / "scripts/release.py")
release = importlib.util.module_from_spec(spec)
spec.loader.exec_module(release)


class ReleaseTests(unittest.TestCase):
    def test_versions_are_numeric_and_ignore_update_archives(self):
        refs = [{"ref": f"refs/tags/{tag}"} for tag in [
            "latest", "build-200-1", "v0.9.0", "v0.10.2", "v0.10.1", "v1.0.0-beta.1",
        ]]
        self.assertEqual(release.latest_version(refs), (0, 10, 2))
        self.assertIsNone(release.latest_version([]))
        self.assertEqual(release.parse_version("v0.2.0"), (0, 2, 0))

    def test_invalid_versions_fail_before_any_remote_calls(self):
        for version in ["0.2", "v0.2.0-beta", "01.2.3", "0.2.0\n", "0.2.0; echo oops"]:
            with self.subTest(version=version), patch.object(release, "command") as command:
                with patch("sys.argv", ["release.py", version]), self.assertRaises(ValueError):
                    release.main()
                command.assert_not_called()

    def test_duplicate_and_older_versions_do_not_push(self):
        refs = [{"ref": "refs/tags/v0.2.0"}]
        for version, message in [((0, 2, 0), "already exists"), ((0, 1, 9), "newer than")]:
            with self.subTest(version=version), patch.object(release, "command") as command:
                with self.assertRaisesRegex(ValueError, message):
                    release.create_release("Plonq/gander", "origin", version, refs)
                command.assert_not_called()

    def test_prompt_suggests_next_patch_including_tags_still_building(self):
        refs = [{"ref": "refs/tags/v0.2.9"}]
        with patch("sys.argv", ["release.py"]), \
                patch.object(release.shutil, "which", return_value="gh"), \
                patch.object(release, "command", return_value="git@github.com:Plonq/gander.git"), \
                patch.object(release, "gh", return_value="Plonq/gander"), \
                patch.object(release, "pages", return_value=refs), \
                patch("builtins.input", return_value="") as prompt, \
                patch.object(release, "create_release") as create, \
                contextlib.redirect_stdout(io.StringIO()):
            release.main()
        self.assertIn("0.2.10", prompt.call_args.args[0])
        self.assertEqual(create.call_args.args[2], (0, 2, 10))

    def test_github_read_errors_do_not_create_tags(self):
        with patch("sys.argv", ["release.py", "0.2.0"]), \
                patch.object(release.shutil, "which", return_value="gh"), \
                patch.object(release, "command", return_value="origin"), \
                patch.object(release, "gh", side_effect=subprocess.CalledProcessError(1, ["gh"])), \
                patch.object(release, "create_release") as create:
            with self.assertRaises(subprocess.CalledProcessError):
                release.main()
        create.assert_not_called()

    def test_push_releases_remote_main_and_preserves_local_work(self):
        with tempfile.TemporaryDirectory() as directory:
            directory = Path(directory)
            remote = directory / "remote.git"
            checkout = directory / "checkout"
            checkout.mkdir()

            def git(*args, cwd=checkout):
                return subprocess.run(
                    ["git", *args], cwd=cwd, check=True, capture_output=True, text=True
                ).stdout.strip()

            git("init", "--bare", str(remote))
            git("init", "-b", "main")
            git("config", "user.name", "Release test")
            git("config", "user.email", "release@example.invalid")
            (checkout / "file").write_text("main\n")
            git("add", "file")
            git("commit", "-m", "Main")
            commit = git("rev-parse", "HEAD")
            git("remote", "add", "origin", str(remote))
            git("push", "origin", "main")
            git("checkout", "-b", "feature")
            (checkout / "file").write_text("feature\n")
            git("commit", "-am", "Unreleased local work")
            local_commit = git("rev-parse", "HEAD")
            (checkout / "file").write_text("unfinished work\n")

            with patch.object(release, "ROOT", checkout), contextlib.redirect_stdout(io.StringIO()):
                release.create_release("example/test", str(remote), (0, 2, 0), [])

            self.assertEqual(git("rev-parse", "refs/tags/v0.2.0", cwd=remote), commit)
            self.assertEqual(git("rev-parse", "HEAD"), local_commit)
            self.assertEqual(git("branch", "--show-current"), "feature")
            self.assertEqual(git("tag", "--list"), "")
            self.assertEqual((checkout / "file").read_text(), "unfinished work\n")

    def test_bundle_uses_tag_version_and_preserves_ci_build_numbers(self):
        with tempfile.TemporaryDirectory() as directory:
            directory = Path(directory)
            (directory / "scripts").mkdir()
            shutil.copy(ROOT / "scripts/bundle-macos.sh", directory / "scripts/bundle-macos.sh")
            (directory / "crates/app").mkdir(parents=True)
            (directory / "crates/app/Cargo.toml").write_text('version = "0.1.0"\n')
            binary = directory / "target/aarch64-apple-darwin/release/gander"
            binary.parent.mkdir(parents=True)
            binary.write_bytes(b"test binary without Sparkle")
            (directory / "assets").mkdir()
            (directory / "assets/Gander.icns").write_bytes(b"test icon")
            env = dict(os.environ, GITHUB_RUN_NUMBER="200", GITHUB_RUN_ATTEMPT="3")
            for version in [None, "0.2.0"]:
                with self.subTest(version=version):
                    env.pop("GANDER_VERSION", None)
                    if version is not None:
                        env["GANDER_VERSION"] = version
                    subprocess.run(
                        ["bash", str(directory / "scripts/bundle-macos.sh")],
                        env=env, check=True, capture_output=True,
                    )
                    with (directory / "dist/Gander.app/Contents/Info.plist").open("rb") as file:
                        info = plistlib.load(file)
                    self.assertEqual(info["CFBundleShortVersionString"], version or "0.1.0")
                    self.assertEqual(info["CFBundleVersion"], "200.3")


if __name__ == "__main__":
    unittest.main()
