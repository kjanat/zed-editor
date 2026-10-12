#!/usr/bin/env python3
"""Exercise the Sentry destination checks in the Linux and Windows bundle scripts."""

import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import unittest


SCRIPTS = Path(__file__).resolve().parent
DESTINATION = {"SENTRY_ORG": "example-org", "SENTRY_PROJECT": "example-project"}


class BundleLinuxTests(unittest.TestCase):
    def run_bundle(self, environment):
        temporary = tempfile.TemporaryDirectory(prefix="bundle-linux-test-")
        self.addCleanup(temporary.cleanup)
        root = Path(temporary.name)
        (root / "script" / "lib").mkdir(parents=True)
        (root / "crates" / "zed").mkdir(parents=True)
        (root / "tools").mkdir()
        (root / "script" / "lib" / "blob-store.sh").touch()
        (root / "crates" / "zed" / "RELEASE_CHANNEL").write_text("stable\n")
        shutil.copyfile(SCRIPTS / "bundle-linux", root / "script" / "bundle-linux")
        build_start = root / "script" / "get-crate-version"
        build_start.write_text("#!/bin/sh\ntouch build-started\nexit 1\n")
        build_start.chmod(0o755)
        sentry_cli = root / "tools" / "sentry-cli"
        sentry_cli.write_text("#!/bin/sh\nexit 0\n")
        sentry_cli.chmod(0o755)
        result = subprocess.run(
            ["bash", "script/bundle-linux"],
            cwd=root,
            env={
                "PATH": str(root / "tools") + os.pathsep + os.environ["PATH"],
                "SENTRY_AUTH_TOKEN": "test-placeholder",
                **environment,
            },
            capture_output=True,
            text=True,
        )
        return result, (root / "build-started").exists()

    def test_upload_without_destination_fails_before_building(self):
        for missing in DESTINATION:
            with self.subTest(missing=missing):
                environment = {
                    name: value
                    for name, value in DESTINATION.items()
                    if name != missing
                }
                result, build_started = self.run_bundle(environment)
                self.assertEqual(result.returncode, 1)
                self.assertIn(
                    "Sentry upload requires SENTRY_ORG and SENTRY_PROJECT.",
                    result.stderr,
                )
                self.assertFalse(build_started)

    def test_upload_with_destination_starts_building(self):
        _, build_started = self.run_bundle(DESTINATION)
        self.assertTrue(build_started)


@unittest.skipUnless(shutil.which("pwsh"), "pwsh is not installed")
class BundleWindowsTests(unittest.TestCase):
    def check_environment(self, environment):
        source = (SCRIPTS / "bundle-windows.ps1").read_text()
        match = re.search(
            r"^function CheckEnvironmentVariables \{\n.*?^\}\n",
            source,
            re.MULTILINE | re.DOTALL,
        )
        if match is None:
            self.fail("bundle-windows.ps1 has no CheckEnvironmentVariables")
        return subprocess.run(
            [
                "pwsh",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                match.group(0) + "CheckEnvironmentVariables\n",
            ],
            env={
                "PATH": os.environ["PATH"],
                "HOME": os.environ["HOME"],
                "CI": "true",
                "ZED_WORKSPACE": "C:\\zed",
                "RELEASE_VERSION": "1.0.0",
                "ZED_RELEASE_CHANNEL": "stable",
                **environment,
            },
            capture_output=True,
            text=True,
        )

    def test_upload_without_destination_fails(self):
        for missing in DESTINATION:
            with self.subTest(missing=missing):
                environment = {
                    name: value
                    for name, value in DESTINATION.items()
                    if name != missing
                }
                result = self.check_environment({
                    "SENTRY_AUTH_TOKEN": "test-placeholder",
                    **environment,
                })
                self.assertEqual(result.returncode, 1)
                self.assertIn(f"Sentry upload requires {missing}", result.stderr)

    def test_upload_with_destination_passes(self):
        result = self.check_environment({
            "SENTRY_AUTH_TOKEN": "test-placeholder",
            **DESTINATION,
        })
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_no_token_needs_no_destination(self):
        result = self.check_environment({})
        self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == "__main__":
    unittest.main()
