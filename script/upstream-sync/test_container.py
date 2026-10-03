"""Exercise a malicious formatter inside the sync container."""

import json
import os
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path


@unittest.skipUnless(
    os.environ.get("SYNC_TEST_IMAGE"),
    "Set SYNC_TEST_IMAGE to run the container regression",
)
class ContainerTests(unittest.TestCase):
    def test_root_formatters_work_offline_as_the_runner_user(self):
        repository = Path(__file__).resolve().parents[2]
        result = subprocess.run(
            [
                "docker",
                "run",
                "--rm",
                "--network=none",
                "--user",
                f"{os.getuid()}:{os.getgid()}",
                "--cap-drop=ALL",
                "--security-opt=no-new-privileges",
                "--mount",
                f"type=bind,src={repository},dst=/source,readonly",
                os.environ["SYNC_TEST_IMAGE"],
                "bash",
                "-euo",
                "pipefail",
                "-c",
                """
mkdir -p /tmp/work/.cargo
cd /tmp/work
cp /source/.dprint.jsonc /source/tombi.toml /source/rustfmt.toml /source/rust-toolchain.toml .
cp /source/.cargo/config.toml .cargo/
test "$DPRINT_CACHE_DIR" = /opt/dprint-cache
test -w "$DPRINT_CACHE_DIR"
dprint output-resolved-config >/dev/null
printf '{"value":1}\n' | dprint fmt --stdin probe.json >/dev/null
printf 'value=1\n' | dprint fmt --stdin probe.toml >/dev/null
printf 'fn main(){}\n' | dprint fmt --stdin probe.rs >/dev/null
tombi --version
rustfmt --version
""",
            ],
            capture_output=True,
            check=False,
            text=True,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_formatter_cannot_read_runner_credentials_or_modify_trusted_source(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source, upstream, output = (
                root / name for name in ("source", "upstream", "output")
            )
            source.mkdir()
            output.mkdir()

            def git(*arguments: str, cwd: Path = source) -> None:
                _ = subprocess.run(
                    ["git", *arguments], cwd=cwd, check=True, capture_output=True
                )

            git("init", "-q", "-b", "master")
            git("config", "user.name", "Test")
            git("config", "user.email", "test@example.invalid")
            git("config", "commit.gpgsign", "false")
            script = source / "script/upstream-sync/prepare.sh"
            script.parent.mkdir(parents=True)
            _ = shutil.copyfile(Path(__file__).with_name("prepare.sh"), script)
            _ = (source / "example.txt").write_text("original\n")
            git("add", ".")
            git("commit", "-qm", "baseline")
            git("clone", "-q", str(source), str(upstream))
            git("config", "user.name", "Test", cwd=upstream)
            git("config", "user.email", "test@example.invalid", cwd=upstream)
            git("config", "commit.gpgsign", "false", cwd=upstream)
            git("branch", "-m", "main", cwd=upstream)
            configuration = {
                "plugins": [
                    "npm:@dprint/exec@0.7.3/plugin.json@704701df449dd7e942a71144773778ac529d68c2e4657bfc236d393b898b9a67"
                ],
                "exec": {
                    "commands": [{"exts": ["txt"], "command": "sh /tmp/work/probe.sh"}]
                },
            }
            _ = (upstream / ".dprint.json").write_text(json.dumps(configuration))
            _ = (upstream / "probe.sh").write_text(
                "set -eu\n"
                + 'test -z "${GH_TOKEN:-}"\n'
                + 'test -z "${SYNC_TOKEN:-}"\n'
                + 'test -z "${ACTIONS_RUNTIME_TOKEN:-}"\n'
                + "test ! -S /var/run/docker.sock\n"
                + "test ! -e /source/.dprint.json\n"
                + "if touch /source/compromised 2>/dev/null; then exit 1; fi\n"
                + "printf safe > /output/formatter-ran\n"
                + "cat\n"
            )
            _ = (upstream / "example.txt").write_text("upstream\n")
            git("add", ".", cwd=upstream)
            git("commit", "-qm", "untrusted formatter", cwd=upstream)
            result = subprocess.run(
                [
                    "docker",
                    "run",
                    "--rm",
                    "--user",
                    f"{os.getuid()}:{os.getgid()}",
                    "--cap-drop=ALL",
                    "--security-opt=no-new-privileges",
                    "--mount",
                    f"type=bind,src={source},dst=/source,readonly",
                    "--mount",
                    f"type=bind,src={upstream},dst=/upstream,readonly",
                    "--mount",
                    f"type=bind,src={output},dst=/output",
                    "--env",
                    "UPSTREAM_URL=/upstream",
                    os.environ["SYNC_TEST_IMAGE"],
                    "bash",
                    "/source/script/upstream-sync/prepare.sh",
                ],
                env={
                    **os.environ,
                    "GH_TOKEN": "synthetic-canary",
                    "SYNC_TOKEN": "synthetic-canary",
                    "ACTIONS_RUNTIME_TOKEN": "synthetic-canary",
                },
                capture_output=True,
                check=False,
                text=True,
            )
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
            self.assertEqual((output / "formatter-ran").read_text(), "safe")
            self.assertEqual((output / "result").read_text(), "clean\n")
            self.assertTrue((output / "sync.bundle").is_file())
            self.assertFalse((source / "compromised").exists())

    def test_conflict_report_fits_the_exporter_limit(self):
        names = [f"conflict-{index:02}-é.txt" for index in range(40)]
        output, _ = self.run_sync(
            {name: "".join(f"{line}\n" for line in range(300)) for name in names},
            {
                name: "".join(f"fork ✓ {line}\n" for line in range(300))
                for name in names
            },
            {
                name: "".join(f"upstream ✗ {line}\n" for line in range(300))
                for name in names
            },
        )
        self.assertEqual((output / "result").read_text(), "partial\n")
        report = (output / "conflict-report.md").read_bytes()
        self.assertLessEqual(len(report), 60000)
        text = report.decode("utf-8")
        self.assertIn(names[-1], text)
        self.assertIn("were omitted", text)
        self.assertIn("## Resolve", text)

    def test_partial_merge_commits_markers_and_keeps_fences_in_the_report(self):
        base = "```json\n{}\n```\nintro\n"
        output, source = self.run_sync(
            {"guide.md": base, "clean.txt": "base\n"},
            {"guide.md": base.replace("intro", "fork intro")},
            {
                "guide.md": base.replace("intro", "upstream intro"),
                "clean.txt": "upstream\n",
            },
        )
        self.assertEqual((output / "result").read_text(), "partial\n")
        text = (output / "conflict-report.md").read_text(encoding="utf-8")
        opening = "````diff\n"
        start = text.index(opening) + len(opening)
        block = text[start : text.index("\n````\n", start)]
        self.assertIn("  ```\n", block)
        self.assertIn("upstream intro", block)
        self.assertIn("<<<<<<< ", self.candidate_file(output, source, "guide.md") or "")
        self.assertEqual(self.candidate_file(output, source, "clean.txt"), "upstream\n")

    def test_structured_merge_resolves_neighbouring_rust_items(self):
        base = "fn first() {}\n\nfn last() {}\n"
        output, source = self.run_sync(
            {"lib.rs": base},
            {"lib.rs": base.replace("\nfn last", "\nfn fork() {}\n\nfn last")},
            {"lib.rs": base.replace("\nfn last", "\nfn upstream() {}\n\nfn last")},
        )
        self.assertEqual((output / "result").read_text(), "resolved\n")
        self.assertEqual((output / "structured.txt").read_text(), "lib.rs\n")
        merged = self.candidate_file(output, source, "lib.rs") or ""
        self.assertIn("fn fork()", merged)
        self.assertIn("fn upstream()", merged)
        self.assertNotIn("<<<<<<<", merged)

    def test_fork_deletion_wins_over_upstream_modification(self):
        output, source = self.run_sync(
            {"gone.txt": "base\n"},
            {"gone.txt": None},
            {"gone.txt": "upstream\n"},
        )
        self.assertEqual((output / "result").read_text(), "resolved\n")
        self.assertEqual((output / "fork-deleted.txt").read_text(), "gone.txt\n")
        self.assertIsNone(self.candidate_file(output, source, "gone.txt"))

    def candidate_file(self, output: Path, source: Path, name: str) -> str | None:
        _ = subprocess.run(
            ["git", "fetch", "-q", str(output / "sync.bundle"), "sync/upstream"],
            cwd=source,
            check=True,
            capture_output=True,
        )
        shown = subprocess.run(
            ["git", "show", f"FETCH_HEAD:{name}"],
            cwd=source,
            capture_output=True,
            check=False,
            text=True,
        )
        return shown.stdout if shown.returncode == 0 else None

    def run_sync(
        self,
        base: dict[str, str],
        fork: dict[str, str | None],
        upstream_files: dict[str, str | None],
    ) -> tuple[Path, Path]:
        repository = Path(__file__).resolve().parents[2]
        root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        source, upstream, output = (
            root / name for name in ("source", "upstream", "output")
        )
        source.mkdir()
        output.mkdir()

        def git(*arguments: str, cwd: Path = source) -> None:
            _ = subprocess.run(
                ["git", *arguments], cwd=cwd, check=True, capture_output=True
            )

        git("init", "-q", "-b", "master")
        git("config", "user.name", "Test")
        git("config", "user.email", "test@example.invalid")
        git("config", "commit.gpgsign", "false")
        script = source / "script/upstream-sync/prepare.sh"
        script.parent.mkdir(parents=True)
        _ = shutil.copyfile(Path(__file__).with_name("prepare.sh"), script)
        for name in (
            ".dprint.jsonc",
            "tombi.toml",
            "rustfmt.toml",
            "rust-toolchain.toml",
            ".cargo/config.toml",
        ):
            (source / name).parent.mkdir(parents=True, exist_ok=True)
            _ = shutil.copyfile(repository / name, source / name)
        for name, contents in base.items():
            _ = (source / name).write_text(contents)
        git("add", ".")
        git("commit", "-qm", "baseline")
        git("clone", "-q", str(source), str(upstream))
        git("config", "user.name", "Test", cwd=upstream)
        git("config", "user.email", "test@example.invalid", cwd=upstream)
        git("config", "commit.gpgsign", "false", cwd=upstream)
        git("branch", "-m", "main", cwd=upstream)
        for directory, files in ((source, fork), (upstream, upstream_files)):
            for name, contents in files.items():
                if contents is None:
                    (directory / name).unlink()
                else:
                    _ = (directory / name).write_text(contents)
            git("commit", "-qam", "rewrite", cwd=directory)
        result = subprocess.run(
            [
                "docker",
                "run",
                "--rm",
                "--network=none",
                "--user",
                f"{os.getuid()}:{os.getgid()}",
                "--cap-drop=ALL",
                "--security-opt=no-new-privileges",
                "--mount",
                f"type=bind,src={source},dst=/source,readonly",
                "--mount",
                f"type=bind,src={upstream},dst=/upstream,readonly",
                "--mount",
                f"type=bind,src={output},dst=/output",
                "--env",
                "UPSTREAM_URL=/upstream",
                os.environ["SYNC_TEST_IMAGE"],
                "bash",
                "/source/script/upstream-sync/prepare.sh",
            ],
            capture_output=True,
            check=False,
            text=True,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        return output, source


if __name__ == "__main__":
    _ = unittest.main()
