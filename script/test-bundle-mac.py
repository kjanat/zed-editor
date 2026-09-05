#!/usr/bin/env python3
"""Exercise macOS build and bundle preparation with simulated platform tools."""

import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest


MOCK_TOOL = r"""
import os
from pathlib import Path
import shutil
import sys

root = Path(os.environ["BUNDLE_TEST_ROOT"])
command = Path(sys.argv[0]).name
arguments = sys.argv[1:]
with (root / "commands").open("a") as log:
    log.write(command + " " + " ".join(arguments) + "\n")
if command == os.environ.get("BUNDLE_TEST_FAILURE"):
    sys.exit(1)

if command == "rustc":
    print("host: aarch64-apple-darwin")
elif command == "cargo":
    if "--help" in arguments:
        print("cargo-bundle v0.6.1-zed")
        sys.exit(0)
    target = arguments[arguments.index("--target") + 1]
    profile = "release" if "--release" in arguments else "debug"
    output = root / "target" / target / profile
    output.mkdir(parents=True, exist_ok=True)
    if "build" in arguments:
        if os.environ.get("BUNDLE_TEST_FAILURE") == "build":
            sys.exit(1)
        assert arguments[:2] == ["--config", ".cargo/bundle-config.toml"]
        packages = [arguments[i + 1] for i, value in enumerate(arguments) if value == "--package"]
        for package in packages:
            (output / package).write_text("unstripped " + package)
    elif "bundle" in arguments:
        if os.environ.get("BUNDLE_TEST_FAILURE") == "bundle":
            sys.exit(1)
        if os.environ.get("CARGO_BUNDLE_SKIP_BUILD") != "true":
            with (root / "commands").open("a") as log:
                log.write("unexpected rebuild\n")
            (output / "zed").write_text("rebuilt zed")
        bundle = output / "bundle" / "osx" / "Zed.app"
        (bundle / "Contents" / "MacOS").mkdir(parents=True)
        shutil.copyfile(output / "zed", bundle / "Contents" / "MacOS" / "zed")
        print(bundle)
elif command == "dsymutil":
    binary = Path(arguments[-1])
    assert binary.read_text().startswith("unstripped ")
    Path(str(binary) + ".dwarf").write_text("symbols " + binary.name)
elif command == "strip":
    binary = Path(arguments[-1])
    if binary.name != "cli":
        assert Path(str(binary) + ".dwarf").exists()
    binary.write_text("stripped " + binary.name)
elif command == "sentry-cli":
    for name in ["zed", "remote_server"]:
        directory = root / "target" / "aarch64-apple-darwin" / "release"
        assert (directory / name).read_text() == "unstripped " + name
        assert (directory / (name + ".dwarf")).exists()
"""


class BundleMacTests(unittest.TestCase):
    def run_bundle(self, options=(), sentry=False, failure=None):
        temporary = tempfile.TemporaryDirectory(prefix="bundle-mac-test-")
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        (root / "script" / "lib").mkdir(parents=True)
        (root / "crates" / "zed").mkdir(parents=True)
        (root / "script" / "lib" / "blob-store.sh").touch()
        licenses = root / "script" / "generate-licenses"
        licenses.write_text("#!/bin/sh\nexit 0\n")
        licenses.chmod(0o755)
        (root / "crates" / "zed" / "RELEASE_CHANNEL").write_text("stable\n")
        (root / "crates" / "zed" / "Cargo.toml").write_text(
            '[package.metadata.bundle-stable]\nname = "Zed"\n'
        )

        tools = root / "tools"
        tools.mkdir()
        for command in ["cargo", "rustc", "rustup", "dsymutil", "strip", "sentry-cli"]:
            tool = tools / command
            tool.write_text(f"#!{sys.executable}\n" + MOCK_TOOL)
            tool.chmod(0o755)

        # Signing and DMG creation require macOS; run the actual preparation
        # section so its ordering and error propagation are checked on Linux too.
        source = Path(__file__).with_name("bundle-mac").read_text()
        preparation = source.split("# DocumentTypes.plist references", 1)[0]
        environment = {
            "PATH": str(tools) + os.pathsep + os.environ["PATH"],
            "BUNDLE_TEST_ROOT": str(root),
        }
        if sentry:
            environment["SENTRY_AUTH_TOKEN"] = "test-placeholder"
        if failure:
            environment["BUNDLE_TEST_FAILURE"] = failure
        result = subprocess.run(
            [shutil.which("bash"), "-c", preparation, "bundle-mac", *options],
            cwd=root,
            env=environment,
            capture_output=True,
            text=True,
        )
        commands = (root / "commands").read_text().splitlines()
        return root, result, commands

    def assert_release_bundle(self, target, sentry=False):
        root, result, commands = self.run_bundle([target], sentry=sentry)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn("unexpected rebuild", commands)
        builds = [line for line in commands if line.startswith("cargo --config")]
        self.assertEqual(len(builds), 2)
        self.assertIn("--package zed --package cli", builds[0])
        self.assertIn("--package remote_server", builds[1])
        self.assertNotIn("--package zed", builds[1])
        bundle_index = next(
            i for i, line in enumerate(commands) if line.startswith("cargo bundle")
        )
        for tool, count in [("dsymutil", 2), ("strip", 3)]:
            indices = [
                i for i, line in enumerate(commands) if line.startswith(tool + " ")
            ]
            self.assertEqual(len(indices), count)
            self.assertLess(max(indices), bundle_index)
        output = root / "target" / target / "release"
        self.assertEqual(
            (output / "bundle/osx/Zed.app/Contents/MacOS/zed").read_text(),
            "stripped zed",
        )
        for name in ["zed", "remote_server"]:
            self.assertEqual(
                (output / (name + ".dwarf")).read_text(), "symbols " + name
            )
        if sentry:
            sentry_index = next(
                i for i, line in enumerate(commands) if line.startswith("sentry-cli ")
            )
            strip_index = next(
                i for i, line in enumerate(commands) if line.startswith("strip ")
            )
            self.assertLess(sentry_index, strip_index)

    def test_release_architectures(self):
        for target in ["aarch64-apple-darwin", "x86_64-apple-darwin"]:
            with self.subTest(target=target):
                self.assert_release_bundle(target)

    def test_upload_symbols_before_stripping(self):
        self.assert_release_bundle("aarch64-apple-darwin", sentry=True)

    def test_debug_and_local_install_keep_symbols(self):
        for option in ["-d", "-i"]:
            with self.subTest(option=option):
                root, result, commands = self.run_bundle([option])
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertNotIn("unexpected rebuild", commands)
                self.assertFalse(
                    any(line.startswith(("strip ", "dsymutil ")) for line in commands)
                )
                profile = "debug" if option == "-d" else "release"
                binary = (
                    root
                    / "target/aarch64-apple-darwin"
                    / profile
                    / "bundle/osx/Zed.app/Contents/MacOS/zed"
                )
                self.assertEqual(binary.read_text(), "unstripped zed")

    def test_preparation_failures_prevent_bundling(self):
        for failure in ["build", "dsymutil", "strip", "sentry-cli"]:
            with self.subTest(failure=failure):
                root, result, commands = self.run_bundle(sentry=True, failure=failure)
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(
                    any(line.startswith("cargo bundle") for line in commands)
                )
                self.assertFalse(list(root.glob("target/*/*/bundle")))

    def test_bundle_failure_propagates(self):
        _, result, _ = self.run_bundle(failure="bundle")
        self.assertNotEqual(result.returncode, 0)


if __name__ == "__main__":
    unittest.main()
