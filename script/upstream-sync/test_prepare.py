"""Exercise merge normalization with the real formatter and Git merge."""

import json
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path


class ForkRulesTests(unittest.TestCase):
    def test_rules_conflicts_keep_fork_and_clean_changes_merge(self):
        script = Path(__file__).resolve().with_name("prepare.sh")
        cases = (
            ("base\n", "fork\n", "upstream\n", True),
            ("base\n", "base\n", "clean upstream edit\n", False),
            ("base\n", "fork\n", None, True),
            ("base\n", None, "upstream\n", True),
            (None, None, "upstream addition\n", False),
            (None, "fork addition\n", "upstream addition\n", True),
        )
        for base, ours, theirs, conflict in cases:
            with self.subTest(base=base, ours=ours, theirs=theirs):
                with tempfile.TemporaryDirectory() as temporary:
                    directory = Path(temporary)

                    def git(*arguments, check=True):
                        return subprocess.run(
                            ["git", "-c", "core.hooksPath=/dev/null", *arguments],
                            cwd=directory,
                            check=check,
                            text=True,
                            capture_output=True,
                        )

                    def commit(contents):
                        rules = directory / ".rules"
                        if contents is None:
                            rules.unlink(missing_ok=True)
                        else:
                            rules.write_text(contents)
                        git("add", "-A")
                        git(
                            "-c",
                            "commit.gpgsign=false",
                            "commit",
                            "--allow-empty",
                            "-m",
                            "fixture",
                        )

                    git("init", "-b", "master")
                    git("config", "user.name", "Test")
                    git("config", "user.email", "test@example.com")
                    commit(base)
                    git("branch", "upstream")
                    commit(ours)
                    git("switch", "upstream")
                    commit(theirs)
                    git("switch", "master")
                    merge = git(
                        "merge", "--no-ff", "--no-commit", "upstream", check=False
                    )
                    self.assertEqual(merge.returncode, 1 if conflict else 0)
                    subprocess.run(
                        ["bash", str(script), "--restore-fork-rules"],
                        cwd=directory,
                        check=True,
                        capture_output=True,
                    )
                    self.assertEqual(git("ls-files", "-u").stdout, "")
                    git(
                        "diff",
                        "--exit-code",
                        "master" if conflict else "upstream",
                        "--",
                        ".rules",
                    )
                    rules = directory / ".rules"
                    self.assertEqual(
                        rules.read_text() if rules.exists() else None,
                        ours if conflict else theirs,
                    )


@unittest.skipUnless(
    shutil.which("dprint") or os.environ.get("SYNC_TEST_IMAGE"),
    "Requires dprint or SYNC_TEST_IMAGE",
)
class MergeFormattingTests(unittest.TestCase):
    def test_independent_settings_change_survives_array_layout_changes(self):
        base = """{
  // Keep project fixtures out of scans.
  "formatter": "auto",
  "ensure_final_newline_on_save": true,
  "file_scan_exclusions": ["fixtures", ".git"],
  "read_only_files": ["**/.rustup/**", "**/.cargo/**"]
}
"""
        ours = base.replace('"auto"', '"dprint"').replace(
            '["**/.rustup/**", "**/.cargo/**"]',
            '[\n    "**/.rustup/**",\n    "**/.cargo/**",\n  ]',
        )
        theirs = base.replace('["fixtures", ".git"]', '["...", "fixtures"]')
        expected = theirs.replace('"auto"', '"dprint"')
        self.check_merge(base, ours, theirs, expected)

    def test_conflicting_settings_values_still_require_resolution(self):
        base = '{"formatter": "auto"}\n'
        self.check_merge(
            base,
            base.replace('"auto"', '"dprint"'),
            base.replace('"auto"', '"prettier"'),
            None,
        )

    def check_merge(
        self, base: str, ours: str, theirs: str, expected: str | None
    ) -> None:
        repository = Path(__file__).resolve().parents[2]
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            image = os.environ.get("SYNC_TEST_IMAGE")
            if image:
                command = [
                    "docker",
                    "run",
                    "--rm",
                    "-i",
                    "--user",
                    f"{os.getuid()}:{os.getgid()}",
                    "--volume",
                    f"{repository}:/repository:ro",
                    "--volume",
                    f"{directory}:/work",
                    "--workdir",
                    "/work",
                    image,
                ]
                script = "/repository/script/upstream-sync/prepare.sh"
                configuration = "/repository/.dprint.jsonc"
            else:
                command = []
                script = str(repository / "script/upstream-sync/prepare.sh")
                configuration = str(repository / ".dprint.jsonc")
            _ = (directory / ".dprint.jsonc").write_text(
                json.dumps({"extends": configuration})
            )

            def normalize(contents: str) -> str:
                result = subprocess.run(
                    [
                        *command,
                        "bash",
                        script,
                        "--format-merge-input",
                        ".zed/settings.json",
                    ],
                    cwd=directory,
                    input=contents,
                    text=True,
                    capture_output=True,
                    check=True,
                )
                self.assertFalse(list(directory.glob(".sync-merge-format.*")))
                return result.stdout

            for name, contents in (("base", base), ("ours", ours), ("theirs", theirs)):
                _ = (directory / name).write_text(normalize(contents))
            result = subprocess.run(
                ["git", "merge-file", "-p", "ours", "base", "theirs"],
                cwd=directory,
                text=True,
                capture_output=True,
                check=False,
            )
            if expected is None:
                self.assertEqual(result.returncode, 1)
                self.assertIn("<<<<<<<", result.stdout)
            else:
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual(result.stdout, normalize(expected))
