"""Exercise the resolver's merge rebuild, in-place check, export and clean-runner verification."""

import json
import os
import shutil
import subprocess
import tempfile
import unittest
from collections.abc import Callable
from itertools import takewhile
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPOSITORY = HERE.parents[1]
SCRIPTS = ("prepare.sh", "resolve.sh", "resolve-prompt.md")
LISTS = (
    "formatting-only.txt",
    "formatted-three-way.txt",
    "structured.txt",
    "lockfiles.txt",
    "fork-deleted.txt",
    "dropped-github.txt",
)
JSON_PLUGIN = "https://plugins.dprint.dev/json-0.25.1.wasm@2d4317b0ce943652edf00e1daea31aa1cc493c4e166816f70e4069dfb5a34695"
MISE_CONFIG = """\
[tools]
dprint = "0.58.0"
nextest = { version = "0.9.146", version_prefix = "cargo-nextest-" }

[tool_alias]
nextest = "github:nextest-rs/nextest"
"""
ALPHA_BASE = """\
pub fn value() -> u32 {
    1
}

pub fn label() -> &'static str {
    "base"
}

#[cfg(test)]
mod tests {
    #[test]
    fn value_is_positive() {
        assert!(super::value() > 0);
    }
}
"""


def toolchain_available() -> bool:
    return all(shutil.which(tool) for tool in ("mise", "cargo"))


def require_toolchain(test: type[unittest.TestCase]) -> type[unittest.TestCase]:
    if toolchain_available() or os.environ.get("CI"):
        return test
    return unittest.skip("Requires mise and cargo")(test)


def git(directory: Path, *arguments: str, check: bool = True) -> str:
    return subprocess.run(
        [
            "git",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            *arguments,
        ],
        cwd=directory,
        check=check,
        text=True,
        capture_output=True,
    ).stdout.strip()


class Fixture:
    """A fork with a conflicting upstream branch and prepare's partial-merge artifact."""

    def __init__(
        self, root: Path, upstream_changes: dict[str, str], fork_changes: dict[str, str]
    ):
        self.root: Path = root
        self.repository: Path = root / "fork"
        self.artifact: Path = root / "artifact"
        self.resolution: Path = root / "resolution"
        self.verifier: Path = root / "verifier"
        self.output: Path = root / "output"
        self.repository.mkdir(parents=True)
        self.artifact.mkdir()
        self.output.mkdir()
        _ = git(self.repository, "init", "-q", "-b", "master")
        _ = git(self.repository, "config", "user.name", "Test")
        _ = git(self.repository, "config", "user.email", "test@example.invalid")
        self.write({
            "Cargo.toml": '[workspace]\nmembers = ["alpha", "beta", "fork_tool"]\nresolver = "2"\n',
            "alpha/Cargo.toml": '[package]\nname = "alpha"\nversion = "0.1.0"\nedition = "2021"\n',
            "alpha/src/lib.rs": ALPHA_BASE,
            "beta/Cargo.toml": '[package]\nname = "beta"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\nalpha = { path = "../alpha" }\n',
            "beta/src/lib.rs": "pub fn doubled() -> u32 {\n    alpha::value() * 2\n}\n",
            "fork_tool/Cargo.toml": '[package]\nname = "fork_tool"\nversion = "0.1.0"\nedition = "2021"\n\n[dependencies]\nalpha = { path = "../alpha" }\n',
            "fork_tool/src/lib.rs": "pub fn shown() -> u32 {\n    alpha::value()\n}\n",
            ".dprint.json": json.dumps({"plugins": [JSON_PLUGIN]}, indent=2) + "\n",
            "data.json": '{ "name": "fixture" }\n',
            ".github/workflows/ci.yml": "trusted workflow\n",
            ".mise.toml": MISE_CONFIG,
            ".gitignore": "/target\n/.sync/\n/.sync-scripts/\n",
            "script/clippy": (REPOSITORY / "script/clippy").read_text(),
        })
        for name in SCRIPTS:
            target = self.repository / "script/upstream-sync" / name
            target.parent.mkdir(parents=True, exist_ok=True)
            _ = shutil.copy2(HERE / name, target)
        _ = subprocess.run(
            ["cargo", "generate-lockfile", "--offline"],
            cwd=self.repository,
            env=self.environment(),
            check=True,
            capture_output=True,
        )
        self.commit("baseline")
        _ = git(self.repository, "branch", "upstream")
        self.write(fork_changes)
        self.commit("fork change")
        _ = git(self.repository, "switch", "-q", "upstream")
        self.write(upstream_changes)
        self.commit("upstream change")
        self.upstream: str = git(self.repository, "rev-parse", "HEAD")
        _ = git(self.repository, "switch", "-q", "master")
        self.fork: str = git(self.repository, "rev-parse", "HEAD")
        self.base: str = git(self.repository, "merge-base", "master", "upstream")
        self.prepare_partial()

    def write(self, files: dict[str, str]) -> None:
        for name, contents in files.items():
            path = self.repository / name
            path.parent.mkdir(parents=True, exist_ok=True)
            _ = path.write_text(contents)

    def commit(self, message: str) -> None:
        _ = git(self.repository, "add", "-A")
        _ = git(self.repository, "commit", "-q", "-m", message)

    def prepare_partial(self) -> None:
        _ = git(self.repository, "switch", "-q", "-c", "sync/upstream")
        merge = subprocess.run(
            [
                "git",
                "-c",
                "core.hooksPath=/dev/null",
                "merge",
                "--no-ff",
                "--no-commit",
                "upstream",
            ],
            cwd=self.repository,
            text=True,
            capture_output=True,
            check=False,
        )
        conflicted = git(
            self.repository, "diff", "--name-only", "--diff-filter=U"
        ).splitlines()
        assert merge.returncode == 1 and conflicted, merge.stdout + merge.stderr
        _ = git(self.repository, "add", "-A")
        _ = git(
            self.repository,
            "commit",
            "-q",
            "-m",
            "Merge upstream with conflicts left for a human",
        )
        _ = git(
            self.repository,
            "bundle",
            "create",
            str(self.artifact / "sync.bundle"),
            "refs/heads/sync/upstream",
            "^master",
        )
        _ = git(self.repository, "switch", "-q", "master")
        _ = git(self.repository, "branch", "-D", "sync/upstream")
        _ = (self.artifact / "result").write_text("partial\n")
        _ = (self.artifact / "human.txt").write_text(
            "".join(f"{path}\n" for path in conflicted)
        )
        _ = (self.artifact / "conflict-report.md").write_text(
            "## Needs a human\n\n" + "".join(f"- `{path}`\n" for path in conflicted)
        )
        for name in LISTS:
            _ = (self.artifact / name).write_text("")

    def environment(self) -> dict[str, str]:
        return {
            **os.environ,
            "GITHUB_ACTIONS": "true",
            "MISE_TRUSTED_CONFIG_PATHS": f"{self.repository}:{self.verifier}",
            "MISE_YES": "1",
        }

    def run(
        self, script: Path, cwd: Path, *arguments: str
    ) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["bash", str(script), *arguments],
            cwd=cwd,
            env=self.environment(),
            text=True,
            capture_output=True,
            check=False,
        )

    def resolve(self, *arguments: str) -> subprocess.CompletedProcess[str]:
        return self.run(
            self.repository / "script/upstream-sync/resolve.sh",
            self.repository,
            *arguments,
        )

    def setup(self) -> subprocess.CompletedProcess[str]:
        result = self.resolve("setup", str(self.artifact))
        assert result.returncode == 0, result.stdout + result.stderr
        return result

    @property
    def state(self) -> Path:
        return self.repository / ".sync"

    def agent(self, files: dict[str, str], tests: str | None = None) -> None:
        for name, contents in files.items():
            path = self.repository / name
            path.parent.mkdir(parents=True, exist_ok=True)
            _ = path.write_text(contents)
            _ = git(self.repository, "add", "--", name)
        _ = (self.state / "resolution.md").write_text(
            "Kept the fork's value and adopted upstream's label.\n"
        )
        if tests is not None:
            _ = (self.state / "tests.txt").write_text(tests)

    def sync_scripts(self, workspace: Path) -> Path:
        target = workspace / ".sync-scripts/script"
        (target / "upstream-sync").mkdir(parents=True, exist_ok=True)
        for name in SCRIPTS:
            _ = shutil.copy2(HERE / name, target / "upstream-sync" / name)
        _ = shutil.copy2(workspace / "script/clippy", target / "clippy")
        return target / "upstream-sync/resolve.sh"

    def export(self) -> None:
        result = self.run(
            self.sync_scripts(self.repository),
            self.repository,
            "export",
            str(self.artifact),
            str(self.resolution),
        )
        assert result.returncode == 0, result.stdout + result.stderr

    def verify(self) -> str:
        _ = git(
            self.root,
            "clone",
            "-q",
            "--no-local",
            str(self.repository),
            str(self.verifier),
        )
        _ = git(self.verifier, "switch", "-q", "--detach", self.fork)
        self.resolution.mkdir(exist_ok=True)
        verified = self.run(
            self.sync_scripts(self.verifier),
            self.verifier,
            "verify",
            str(self.artifact),
            str(self.resolution),
            str(self.output),
        )
        assert verified.returncode == 0, verified.stdout + verified.stderr
        return (self.output / "result").read_text().strip()

    def finish(self) -> str:
        self.export()
        return self.verify()


FORK_ALPHA = ALPHA_BASE.replace("    1\n", "    2\n")
UPSTREAM_ALPHA = ALPHA_BASE.replace("    1\n", "    3\n").replace(
    '"base"', '"upstream"'
)
RESOLVED_ALPHA = ALPHA_BASE.replace("    1\n", "    2\n").replace(
    '"base"', '"upstream"'
)


def conflict_fixture(root: Path) -> Fixture:
    return Fixture(
        root, {"alpha/src/lib.rs": UPSTREAM_ALPHA}, {"alpha/src/lib.rs": FORK_ALPHA}
    )


@require_toolchain
class ResolverTests(unittest.TestCase):
    def fixture(self, factory: Callable[[Path], Fixture] = conflict_fixture) -> Fixture:
        root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        fixture = factory(root)
        _ = fixture.setup()
        return fixture

    def assert_failed(self, fixture: Fixture, reason: str) -> str:
        self.assertEqual(fixture.finish(), "conflict")
        self.assertFalse((fixture.output / "sync.bundle").exists())
        body = (fixture.output / "issue-body.md").read_text()
        self.assertIn(reason, body)
        self.assertIn("alpha/src/lib.rs", body)
        self.assertLessEqual(len(body.encode()), 60000)
        return body

    def test_setup_rebuilds_the_exact_recorded_merge(self):
        root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        fixture = conflict_fixture(root)
        output = fixture.setup().stdout
        for sha in (fixture.fork, fixture.upstream, fixture.base):
            self.assertIn(sha, output)
        self.assertIn("alpha/src/lib.rs", output)
        self.assertEqual(git(fixture.repository, "rev-parse", "HEAD"), fixture.fork)
        self.assertEqual(
            git(fixture.repository, "rev-parse", "MERGE_HEAD"), fixture.upstream
        )
        self.assertEqual(
            git(fixture.repository, "diff", "--name-only", "--diff-filter=U"),
            "alpha/src/lib.rs",
        )
        self.assertIn("<<<<<<<", (fixture.repository / "alpha/src/lib.rs").read_text())
        stages = git(
            fixture.repository, "ls-files", "--stage", "--", "alpha/src/lib.rs"
        )
        self.assertEqual(
            sorted(line.split()[2] for line in stages.splitlines()), ["1", "2", "3"]
        )

    def test_setup_uses_the_recorded_upstream_even_after_upstream_moves(self):
        root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        fixture = conflict_fixture(root)
        _ = git(fixture.repository, "switch", "-q", "upstream")
        fixture.write({"later.txt": "after the artifact\n"})
        fixture.commit("later upstream commit")
        _ = git(fixture.repository, "switch", "-q", "master")
        _ = fixture.setup()
        self.assertEqual(
            git(fixture.repository, "rev-parse", "MERGE_HEAD"), fixture.upstream
        )
        self.assertFalse((fixture.repository / "later.txt").exists())

    def test_setup_refuses_a_candidate_for_another_fork_commit(self):
        root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        fixture = conflict_fixture(root)
        fixture.write({"moved.txt": "master moved\n"})
        fixture.commit("master moved")
        result = fixture.resolve("setup", str(fixture.artifact))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not the trusted fork commit", result.stderr)

    def test_check_runs_in_place_and_names_the_failing_check(self):
        fixture = self.fixture()
        failed = fixture.resolve("check")
        self.assertNotEqual(failed.returncode, 0)
        self.assertIn("alpha/src/lib.rs", failed.stdout)
        self.assertEqual(
            (fixture.state / "failed-check").read_text(), "unmerged paths\n"
        )
        fixture.agent({"alpha/src/lib.rs": RESOLVED_ALPHA})
        passed = fixture.resolve("check")
        self.assertEqual(passed.returncode, 0, passed.stdout + passed.stderr)
        self.assertIn("Every check passed", passed.stdout)
        self.assertFalse((fixture.state / "failed-check").exists())

    def test_verified_resolution_merges_the_fork_and_upstream(self):
        fixture = self.fixture()
        fixture.agent(
            {"alpha/src/lib.rs": RESOLVED_ALPHA},
            tests="alpha\ttest(value_is_positive)\n",
        )
        self.assertEqual(fixture.finish(), "resolved")
        _ = git(
            fixture.verifier,
            "fetch",
            "-q",
            str(fixture.output / "sync.bundle"),
            "sync/upstream:refs/check/resolved",
        )
        head = "refs/check/resolved"
        self.assertEqual(git(fixture.verifier, "rev-parse", f"{head}^1"), fixture.fork)
        self.assertEqual(
            git(fixture.verifier, "rev-parse", f"{head}^2"), fixture.upstream
        )
        self.assertEqual(
            git(fixture.verifier, "log", "-1", "--format=%an", head),
            "github-actions[bot]",
        )
        merged = git(fixture.verifier, "show", f"{head}:alpha/src/lib.rs")
        self.assertNotIn("<<<<<<<", merged)
        self.assertIn('"upstream"', merged)
        resolution = (fixture.output / "resolution.md").read_text()
        self.assertIn("Kept the fork's value", resolution)
        self.assertNotIn("beyond the conflicts", resolution)
        for name in LISTS:
            self.assertTrue((fixture.output / name).is_file(), name)

    def test_changes_beyond_the_conflicts_are_listed(self):
        fixture = self.fixture()
        fixture.agent({
            "alpha/src/lib.rs": RESOLVED_ALPHA,
            "beta/src/lib.rs": "pub fn doubled() -> u32 {\n    alpha::value() + alpha::value()\n}\n",
        })
        self.assertEqual(fixture.finish(), "resolved")
        resolution = (fixture.output / "resolution.md").read_text()
        self.assertIn(
            "### Files changed beyond the conflicts\n\n- `beta/src/lib.rs`", resolution
        )
        self.assertNotIn("- `alpha/src/lib.rs`", resolution)

    def test_trial_export_before_claude_leaves_the_real_export_intact(self):
        root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        fixture = conflict_fixture(root)
        fixture.export()
        shutil.rmtree(fixture.resolution)
        shutil.rmtree(fixture.state)
        _ = fixture.setup()
        fixture.agent({"alpha/src/lib.rs": RESOLVED_ALPHA})
        self.assertEqual(fixture.finish(), "resolved")

    def test_agent_that_does_nothing_produces_no_candidate(self):
        _ = self.assert_failed(self.fixture(), "Claude produced no merge")

    def test_save_step_keeps_everything_when_the_export_fails(self):
        workflow = (
            REPOSITORY / ".github/workflows/fork_upstream_sync.yaml"
        ).read_text()
        lines = workflow.split(
            "name: Save Claude's checkout, notes, logs and session\n", 1
        )[1].splitlines()
        start = lines.index("        run: |") + 1
        script = "\n".join(
            line.removeprefix("          ")
            for line in takewhile(
                lambda line: line.startswith("          "), lines[start:]
            )
        )
        fixture = self.fixture()
        fixture.agent({"alpha/src/lib.rs": RESOLVED_ALPHA})
        _ = (fixture.repository / "notes-draft.txt").write_text("half done\n")
        runner = fixture.root / "runner"
        session = runner / "home/.claude/projects/-work/session.jsonl"
        session.parent.mkdir(parents=True)
        _ = session.write_text('{"token":"secret-oauth-canary"}\n')
        _ = (runner / "claude-execution-output.json").write_text(
            '["secret-oauth-canary printed"]\n'
        )
        result = subprocess.run(
            ["bash", "-e", "-c", script],
            cwd=fixture.repository,
            env={
                "PATH": os.environ["PATH"],
                "HOME": str(runner / "home"),
                "RUNNER_TEMP": str(runner),
                "GIT_INDEX_FILE": str(runner / "claude.index"),
                "TOKEN": "secret-oauth-canary",
            },
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        saved = runner / "sync-resolution"
        patch_text = (saved / "checkout.patch").read_text()
        self.assertIn("notes-draft.txt", patch_text)
        self.assertNotIn(".sync/", patch_text)
        self.assertIn(
            "Kept the fork's value", (saved / "state/resolution.md").read_text()
        )
        self.assertIn("***", (saved / "claude-execution-output.json").read_text())
        copied = (saved / "claude-projects/-work/session.jsonl").read_text()
        self.assertEqual(copied, '{"token":"***"}\n')
        for path in saved.rglob("*"):
            if path.is_file():
                self.assertNotIn(
                    "secret-oauth-canary", path.read_text(errors="replace")
                )

    def test_unfinished_work_survives_unmerged_paths(self):
        fixture = self.fixture()
        _ = (fixture.repository / "alpha/src/lib.rs").write_text(RESOLVED_ALPHA)
        _ = (fixture.repository / "notes.txt").write_text("half done\n")
        fixture.export()
        self.assertFalse((fixture.resolution / "resolution.bundle").exists())
        _ = git(
            fixture.root,
            "clone",
            "-q",
            "--no-local",
            str(fixture.repository),
            str(fixture.verifier),
        )
        _ = git(
            fixture.verifier,
            "fetch",
            "-q",
            str(fixture.resolution / "unfinished.bundle"),
            "refs/sync/unfinished:refs/check/unfinished",
        )
        self.assertEqual(
            git(fixture.verifier, "show", "refs/check/unfinished:alpha/src/lib.rs"),
            RESOLVED_ALPHA.strip(),
        )
        self.assertEqual(
            git(fixture.verifier, "show", "refs/check/unfinished:notes.txt"),
            "half done",
        )

    def test_staged_conflict_markers_produce_no_candidate(self):
        fixture = self.fixture()
        _ = git(fixture.repository, "add", "--", "alpha/src/lib.rs")
        _ = self.assert_failed(fixture, "Validation failed at **conflict markers**")

    def test_compile_failure_produces_no_candidate(self):
        fixture = self.fixture()
        fixture.agent({
            "alpha/src/lib.rs": RESOLVED_ALPHA.replace("    2\n", '    "two"\n')
        })
        _ = self.assert_failed(fixture, "Validation failed at **workspace compile**")

    def test_whole_workspace_breakage_in_clean_merged_code_is_caught(self):
        def renamed(root: Path) -> Fixture:
            upstream = UPSTREAM_ALPHA.replace(
                "pub fn value()", "pub fn amount()"
            ).replace("super::value()", "super::amount()")
            return Fixture(
                root,
                {
                    "alpha/src/lib.rs": upstream,
                    "beta/src/lib.rs": "pub fn doubled() -> u32 {\n    alpha::amount() * 2\n}\n",
                },
                {"alpha/src/lib.rs": FORK_ALPHA},
            )

        fixture = self.fixture(renamed)
        resolved = RESOLVED_ALPHA.replace("pub fn value()", "pub fn amount()").replace(
            "super::value()", "super::amount()"
        )
        fixture.agent({"alpha/src/lib.rs": resolved})
        body = self.assert_failed(fixture, "Validation failed at **workspace compile**")
        self.assertIn("fork_tool", body)

    def test_failing_crate_test_produces_no_candidate(self):
        fixture = self.fixture()
        fixture.agent({
            "alpha/src/lib.rs": RESOLVED_ALPHA.replace(
                "assert!(super::value() > 0)", "assert!(super::value() > 9)"
            )
        })
        _ = self.assert_failed(fixture, "Validation failed at **tests**")

    def test_failing_targeted_test_produces_no_candidate(self):
        fixture = self.fixture()
        fixture.agent(
            {"alpha/src/lib.rs": RESOLVED_ALPHA},
            tests="alpha\ttest(no_such_test_exists)\n",
        )
        _ = self.assert_failed(fixture, "Validation failed at **targeted tests**")

    def test_automation_changes_produce_no_candidate(self):
        for path in (
            ".github/workflows/ci.yml",
            "script/upstream-sync/resolve.sh",
            "script/clippy",
        ):
            with self.subTest(path=path):
                fixture = self.fixture()
                fixture.agent({
                    "alpha/src/lib.rs": RESOLVED_ALPHA,
                    path: "exit 0\n",
                })
                checked = fixture.run(
                    fixture.sync_scripts(fixture.repository),
                    fixture.repository,
                    "check",
                )
                self.assertIn(
                    "automation unchanged",
                    (fixture.state / "failed-check").read_text(),
                    checked.stdout,
                )
                body = self.assert_failed(
                    fixture,
                    f"Claude's merge changes workflow or sync script files:\n\n- `{path}`",
                )
                self.assertNotIn("Validation failed", body)

    def test_resolution_with_other_parents_is_rejected(self):
        fixture = self.fixture()
        fixture.agent({"alpha/src/lib.rs": RESOLVED_ALPHA})
        tree = git(fixture.repository, "write-tree")
        forged = git(
            fixture.repository, "commit-tree", tree, "-p", fixture.fork, "-m", "forged"
        )
        _ = git(fixture.repository, "update-ref", "refs/sync/resolved", forged)
        fixture.resolution.mkdir()
        _ = git(
            fixture.repository,
            "bundle",
            "create",
            str(fixture.resolution / "resolution.bundle"),
            "refs/sync/resolved",
            f"^{fixture.fork}",
        )
        self.assertEqual(fixture.verify(), "conflict")
        self.assertIn(
            f"does not have {fixture.fork} and {fixture.upstream} as its parents",
            (fixture.output / "issue-body.md").read_text(),
        )

    def test_resolver_notes_reach_the_failure_report(self):
        fixture = self.fixture()
        fixture.agent({
            "alpha/src/lib.rs": RESOLVED_ALPHA.replace("    2\n", '    "two"\n')
        })
        _ = (fixture.state / "resolution.md").write_text(
            "Upstream's build.rs downloads a binary.\n```\nfence\n```\n"
        )
        body = self.assert_failed(fixture, "Claude's notes")
        self.assertIn("Upstream's build.rs downloads a binary.", body)
        self.assertIn("````\n", body)

    def test_a_merge_claude_committed_still_exports(self):
        fixture = self.fixture()
        fixture.agent({"alpha/src/lib.rs": RESOLVED_ALPHA})
        _ = git(fixture.repository, "commit", "-q", "--no-edit")
        self.assertFalse((fixture.repository / ".git/MERGE_HEAD").exists())
        self.assertEqual(fixture.finish(), "resolved")

    def test_missing_resolution_still_reports_the_conflict(self):
        root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        fixture = conflict_fixture(root)
        _ = self.assert_failed(fixture, "Claude produced no merge")
        self.assertFalse((fixture.resolution / "resolution.bundle").exists())


class TrustedConfigurationTests(unittest.TestCase):
    workflow: str = (
        REPOSITORY / ".github/workflows/fork_upstream_sync.yaml"
    ).read_text()

    def job(self, name: str) -> str:
        block = self.workflow.split(f"\n  {name}:\n", 1)[1]
        end = next(
            (
                index
                for index, line in enumerate(block.splitlines())
                if line.startswith("  ")
                and not line.startswith("   ")
                and line.strip().endswith(":")
            ),
            None,
        )
        lines = block.splitlines()
        return "\n".join(lines[:end] if end is not None else lines)

    def test_claude_runs_only_for_partial_merges(self):
        self.assertIn(
            "if: needs.prepare.outputs.result == 'partial'", self.job("resolve")
        )
        self.assertEqual(self.workflow.count("claude-code-action"), 1)

    def test_resolver_has_no_github_write_credential(self):
        resolve = self.job("resolve")
        self.assertIn("permissions: { contents: read }", resolve)
        self.assertNotIn("SYNC_TOKEN", resolve)
        self.assertNotIn("github.token", resolve)
        self.assertNotIn("id-token", self.workflow)
        self.assertNotIn("persist-credentials: true", self.workflow)
        self.assertEqual(
            resolve.count("secrets."),
            resolve.count("secrets.CLAUDE_CODE_OAUTH_TOKEN }}"),
        )
        self.assertIn(
            "claude_code_oauth_token: ${{ secrets.CLAUDE_CODE_OAUTH_TOKEN }}", resolve
        )

    def test_claude_gets_the_static_prompt(self):
        resolve = self.job("resolve")
        self.assertRegex(
            resolve,
            r"uses: anthropics/claude-code-action/base-action@[0-9a-f]{40} # v\S+\n",
        )
        self.assertIn(
            "prompt_file: ${{ github.workspace }}/script/upstream-sync/resolve-prompt.md",
            resolve,
        )
        self.assertIn("--model claude-opus-5-5", resolve)
        self.assertIn("--permission-mode auto", resolve)

    def run_line(self, subcommand: str) -> str:
        lines = self.workflow.splitlines()
        script = f"bash .sync-scripts/script/upstream-sync/resolve.sh {subcommand}"
        start = next(
            index
            for index, line in enumerate(lines)
            if line.strip().startswith("run: ") and script in line
        )
        indent = len(lines[start]) - len(lines[start].lstrip())
        command = [script + lines[start].split(script, 1)[1]]
        for line in lines[start + 1 :]:
            if len(line) - len(line.lstrip()) <= indent:
                break
            command.append(line.strip())
        return " ".join(command)

    def test_the_export_command_runs_before_claude(self):
        resolve = self.job("resolve")
        trial = resolve.index("run: &export-resolution ")
        self.assertLess(trial, resolve.index("name: Resolve the conflicts with Claude"))
        self.assertGreater(resolve.index("run: *export-resolution"), trial)

    def test_workflow_passes_every_argument_to_resolve_sh(self):
        for subcommand, expected in (
            ("export", ["export", "{temp}/sync-candidate", "{temp}/sync-resolution"]),
            (
                "verify",
                [
                    "verify",
                    "{temp}/sync-candidate",
                    "{temp}/sync-resolution",
                    "{temp}/sync-output",
                ],
            ),
        ):
            with (
                self.subTest(subcommand=subcommand),
                tempfile.TemporaryDirectory() as temporary,
            ):
                workspace = Path(temporary)
                stub = workspace / ".sync-scripts/script/upstream-sync/resolve.sh"
                stub.parent.mkdir(parents=True)
                _ = stub.write_text('printf "%s\\n" "$@"\n')
                result = subprocess.run(
                    ["bash", "-e", "-c", self.run_line(subcommand)],
                    cwd=workspace,
                    env={
                        "PATH": os.environ["PATH"],
                        "RUNNER_TEMP": temporary,
                        "sync_output": f"{temporary}/sync-output",
                    },
                    text=True,
                    capture_output=True,
                    check=True,
                )
                self.assertEqual(
                    result.stdout.splitlines(),
                    [argument.format(temp=temporary) for argument in expected],
                )

    def test_verification_runs_trusted_scripts_on_a_clean_runner(self):
        verify = self.job("verify")
        self.assertIn("permissions: { contents: read }", verify)
        self.assertNotIn("secrets.", verify)
        self.assertIn("- *sync-scripts", verify)
        self.assertIn(".sync-scripts/script/upstream-sync/resolve.sh verify", verify)
        scripts = self.workflow.split("- &sync-scripts\n", 1)[1].split("\n      - ", 1)[
            0
        ]
        self.assertIn("path: .sync-scripts\n", scripts)
        self.assertIn("ref: ${{ github.sha }}\n", scripts)
        self.assertIn("filter: blob:none\n", scripts)
        self.assertIn(
            "sparse-checkout: |\n            /script/upstream-sync/\n            /script/clippy",
            scripts,
        )
        self.assertIn(
            ".sync-scripts/script/upstream-sync/resolve.sh export", self.job("resolve")
        )
        self.assertIn("name: upstream-sync-verified", verify)
        self.assertIn("upstream-sync-verified", self.job("open-sync-pr"))

    def test_mise_provisions_the_resolver(self):
        resolve = self.job("resolve")
        mise = resolve.split("- &mise\n", 1)[1].split("\n      - ", 1)[0]
        self.assertRegex(mise, r"uses: jdx/mise-action@[0-9a-f]{40} # v\S+\n")
        self.assertIn("bootstrap: true, bootstrap_skip: compose", mise)
        self.assertIn("- *mise", self.job("verify"))
        for name in ("resolve", "verify"):
            job = self.job(name)
            for unmanaged in (
                "setup-node",
                "setup-python",
                "rust-toolchain",
                "rustup ",
                "npm install",
                "pip install",
            ):
                self.assertNotIn(unmanaged, job)


if __name__ == "__main__":
    _ = unittest.main()
