import hashlib
import os
import re
import subprocess
import tarfile
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "freebsd-sysroot"
TOKEN = "ghp_7Kq2mVx9LpR4sTn8WcYe3HbJ6dFa1ZuG0oQi"

STUB = """#!/usr/bin/env bash
name="${0##*/}"
printf '%s %s\\n' "${name}" "$*" >> "${STUB_LOG}"
if [[ " $* " == *" --password-stdin "* ]]; then
    printf 'stdin %s\\n' "$(cat)" >> "${STUB_LOG}"
fi
output=""
while (($#)); do
    if [[ "$1" == --output ]]; then
        output="$2"
        shift
    fi
    shift
done
behavior_variable="${name^^}_BEHAVIOR"
case "${!behavior_variable}" in
    serve) cp "${FIXTURE_ARCHIVE}" "${output}" ;;
    garbage) printf garbage > "${output}" ;;
    *) exit 7 ;;
esac
"""

GH_STUB = """#!/usr/bin/env bash
printf 'gh %s\\n' "$*" >> "${STUB_LOG}"
[[ -n "${GH_AUTH_TOKEN:-}" ]] || exit 1
printf '%s\\n' "${GH_AUTH_TOKEN}"
"""


class FreebsdSysrootTest(unittest.TestCase):
    def setUp(self) -> None:
        temporary = tempfile.TemporaryDirectory(prefix="freebsd-sysroot-test-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.cache = self.root / "cache" / "zed-freebsd-sysroot"
        self.log = self.root / "stub.log"
        stubs = self.root / "stubs"
        stubs.mkdir()
        for name in ("oras", "curl"):
            stub = stubs / name
            stub.write_text(STUB)
            stub.chmod(0o755)
        gh = stubs / "gh"
        gh.write_text(GH_STUB)
        gh.chmod(0o755)
        self.path = f"{stubs}{os.pathsep}{os.environ['PATH']}"

    def archive(self, *directories: str) -> Path:
        tree = self.root / "tree"
        for directory in directories:
            (tree / directory).mkdir(parents=True)
            (tree / directory / "marker").write_text(directory)
        (tree / "lib" / "libc.so.7").write_text("libc")
        archive = self.root / "base.txz"
        with tarfile.open(archive, "w:xz") as output:
            output.add(tree, arcname=".")
        return archive

    def complete_archive(self) -> Path:
        return self.archive("lib", "usr/lib", "usr/include")

    def script_for(self, archive: Path) -> Path:
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        source, count = re.subn(
            r"^base_archive_sha256=.*$",
            f"base_archive_sha256={digest}",
            SCRIPT.read_text(),
            flags=re.MULTILINE,
        )
        self.assertEqual(count, 1)
        script = self.root / "freebsd-sysroot"
        script.write_text(source)
        return script

    def run_script(
        self,
        archive: Path,
        oras: str,
        curl: str,
        token: str | None = None,
        gh_token: str | None = None,
    ) -> subprocess.CompletedProcess[str]:
        environment = {
            "PATH": self.path,
            "HOME": str(self.root),
            "XDG_CACHE_HOME": str(self.root / "cache"),
            "USER": "tester",
            "STUB_LOG": str(self.log),
            "FIXTURE_ARCHIVE": str(archive),
            "ORAS_BEHAVIOR": oras,
            "CURL_BEHAVIOR": curl,
        }
        if token is not None:
            environment["GITHUB_TOKEN"] = token
            environment["GITHUB_ACTOR"] = "kjanat"
        if gh_token is not None:
            environment["GH_AUTH_TOKEN"] = gh_token
        return subprocess.run(
            ["bash", str(self.script_for(archive))],
            env=environment,
            capture_output=True,
            text=True,
        )

    def calls(self) -> list[str]:
        if not self.log.exists():
            return []
        return self.log.read_text().splitlines()

    def sysroot(self) -> Path:
        return self.cache / "13.2-amd64-v1"

    def assert_nothing_left(self) -> None:
        self.assertFalse(self.sysroot().exists())
        self.assertEqual(list(self.cache.glob(".prepare.*")), [])

    def test_registry_copy_prepares_the_sysroot(self) -> None:
        archive = self.complete_archive()
        result = self.run_script(archive, oras="serve", curl="fail")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, f"{self.sysroot()}\n")
        self.assertEqual((self.sysroot() / "lib" / "libc.so.7").read_text(), "libc")
        self.assertTrue((self.sysroot() / "usr" / "include" / "marker").exists())
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        self.assertEqual((self.sysroot() / ".prepared").read_text(), f"{digest}\n")
        self.assertEqual([call.split()[0] for call in self.calls()], ["gh", "oras"])
        self.assertIn(f"ghcr.io/kjanat/freebsd-dist@sha256:{digest}", self.calls()[1])

    def test_unreachable_registry_falls_back_to_the_archive(self) -> None:
        result = self.run_script(self.complete_archive(), oras="fail", curl="serve")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, f"{self.sysroot()}\n")
        self.assertEqual(
            [call.split()[0] for call in self.calls()], ["gh", "oras", "curl"]
        )
        self.assertIn(
            "https://archive.freebsd.org/ftp/old-releases/amd64/13.2-RELEASE/base.txz",
            self.calls()[2],
        )
        self.assertIn("fetch_from_registry did not produce base.txz", result.stderr)

    def test_mismatched_downloads_are_rejected_before_extraction(self) -> None:
        result = self.run_script(
            self.complete_archive(), oras="garbage", curl="garbage"
        )
        self.assertEqual(result.returncode, 1)
        self.assertEqual(result.stdout, "")
        self.assertIn("no source produced FreeBSD 13.2 base.txz", result.stderr)
        self.assert_nothing_left()

    def test_prepared_sysroot_needs_no_source(self) -> None:
        archive = self.complete_archive()
        self.assertEqual(
            self.run_script(archive, oras="serve", curl="fail").returncode, 0
        )
        self.log.unlink()
        result = self.run_script(archive, oras="fail", curl="fail")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, f"{self.sysroot()}\n")
        self.assertEqual(self.calls(), [])

    def test_unmarked_sysroot_is_prepared_again(self) -> None:
        (self.sysroot() / "lib").mkdir(parents=True)
        (self.sysroot() / "lib" / "libc.so.7").write_text("partial")
        archive = self.complete_archive()
        failed = self.run_script(archive, oras="fail", curl="fail")
        self.assertEqual(failed.returncode, 1)
        self.assertEqual(failed.stdout, "")
        result = self.run_script(archive, oras="serve", curl="fail")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.sysroot() / "lib" / "libc.so.7").read_text(), "libc")
        self.assertTrue((self.sysroot() / ".prepared").exists())

    def test_failed_extraction_leaves_no_sysroot(self) -> None:
        result = self.run_script(
            self.archive("lib", "usr/lib"), oras="serve", curl="fail"
        )
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(result.stdout, "")
        self.assert_nothing_left()

    def test_abandoned_preparation_is_removed(self) -> None:
        abandoned = self.cache / ".prepare.abandoned"
        abandoned.mkdir(parents=True)
        result = self.run_script(self.complete_archive(), oras="serve", curl="fail")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(abandoned.exists())

    def test_token_reaches_oras_on_stdin(self) -> None:
        result = self.run_script(
            self.complete_archive(), oras="serve", curl="fail", token=TOKEN
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        invocation, stdin = self.calls()
        self.assertNotIn(TOKEN, invocation)
        self.assertIn("--username kjanat --password-stdin", invocation)
        self.assertEqual(stdin, f"stdin {TOKEN}")

    def test_gh_token_reaches_oras_on_stdin(self) -> None:
        result = self.run_script(
            self.complete_archive(), oras="serve", curl="fail", gh_token=TOKEN
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        lookup, invocation, stdin = self.calls()
        self.assertEqual(lookup, "gh auth token")
        self.assertNotIn(TOKEN, invocation)
        self.assertIn("--username tester --password-stdin", invocation)
        self.assertEqual(stdin, f"stdin {TOKEN}")

    def test_without_any_token_oras_fetches_anonymously(self) -> None:
        result = self.run_script(self.complete_archive(), oras="serve", curl="fail")
        self.assertEqual(result.returncode, 0, result.stderr)
        lookup, invocation = self.calls()
        self.assertEqual(lookup, "gh auth token")
        self.assertNotIn("--password-stdin", invocation)
        self.assertNotIn("--username", invocation)


if __name__ == "__main__":
    unittest.main()
