"""Exercise a malicious formatter using synthetic credentials, never real secrets."""

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


if __name__ == "__main__":
    _ = unittest.main()
