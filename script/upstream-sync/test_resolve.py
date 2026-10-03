"""Exercise the Claude resolver's merge rebuild, sandbox, validation and packaging."""

import json
import os
import shutil
import socket
import subprocess
import tempfile
import textwrap
import unittest
from collections.abc import Callable
from pathlib import Path
from typing import ClassVar, NotRequired, TypedDict, cast, override


class HookDecision(TypedDict):
    permissionDecision: str
    updatedInput: NotRequired[dict[str, str]]


class HookOutput(TypedDict):
    hookSpecificOutput: HookDecision


class HookCommand(TypedDict):
    command: str


class HookMatcher(TypedDict):
    matcher: str
    hooks: list[HookCommand]


class Permissions(TypedDict):
    deny: list[str]


class ClaudeSettings(TypedDict):
    permissions: Permissions
    hooks: dict[str, list[HookMatcher]]
    disableAllHooks: bool
    enableAllProjectMcpServers: bool


HERE = Path(__file__).resolve().parent
REPOSITORY = HERE.parents[1]
SCRIPTS = (
    "prepare.sh",
    "resolve.sh",
    "sandbox-exec",
    "sandbox-hook",
    "resolve-prompt.md",
)
LISTS = (
    "formatting-only.txt",
    "formatted-three-way.txt",
    "structured.txt",
    "lockfiles.txt",
    "fork-deleted.txt",
    "dropped-github.txt",
)
CANARIES = {
    "CLAUDE_CODE_OAUTH_TOKEN": "canary-claude-oauth",
    "ANTHROPIC_API_KEY": "canary-anthropic-key",
    "GITHUB_TOKEN": "canary-github-token",
    "GH_TOKEN": "canary-gh-token",
    "SYNC_TOKEN": "canary-sync-token",
    "ACTIONS_RUNTIME_TOKEN": "canary-actions-runtime",
}
JSON_PLUGIN = "https://plugins.dprint.dev/json-0.25.1.wasm@2d4317b0ce943652edf00e1daea31aa1cc493c4e166816f70e4069dfb5a34695"
MISE_CONFIG = """\
[tools]
node = "24"
dprint = "0.58.0"
srt = "0.0.78"
nextest = { version = "0.9.146", version_prefix = "cargo-nextest-" }

[tool_alias]
nextest = "github:nextest-rs/nextest"
srt = "npm:@anthropic-ai/sandbox-runtime"
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


def sandbox_available() -> bool:
    return all(shutil.which(tool) for tool in ("bwrap", "socat", "rg", "mise", "cargo"))


def require_sandbox(test: type[unittest.TestCase]) -> type[unittest.TestCase]:
    if sandbox_available() or os.environ.get("CI"):
        return test
    return unittest.skip("Requires bubblewrap, socat, ripgrep, mise and cargo")(test)


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
        self.base_dir: Path = root
        self.home: Path = root / "home"
        self.repository: Path = self.home / "work" / "fork"
        self.artifact: Path = root / "artifact"
        self.work: Path = root / "sync"
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
            ".gitignore": "/target\n",
            "script/clippy": (REPOSITORY / "script/clippy").read_text(),
        })
        for name in SCRIPTS:
            target = self.repository / "script/upstream-sync" / name
            target.parent.mkdir(parents=True, exist_ok=True)
            _ = shutil.copy2(HERE / name, target)
        _ = subprocess.run(
            ["cargo", "generate-lockfile", "--offline"],
            cwd=self.repository,
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
        environment = {
            key: value for key, value in os.environ.items() if key not in CANARIES
        }
        real_home = Path.home()
        environment.update({
            "HOME": str(self.home),
            "MISE_DATA_DIR": os.environ.get(
                "MISE_DATA_DIR", str(real_home / ".local/share/mise")
            ),
            "MISE_TRUSTED_CONFIG_PATHS": str(self.repository),
            "RUSTUP_HOME": os.environ.get("RUSTUP_HOME", str(real_home / ".rustup")),
            "CARGO_HOME": os.environ.get("CARGO_HOME", str(real_home / ".cargo")),
        })
        environment.update(CANARIES)
        return environment

    def resolve(self, *arguments: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [
                "bash",
                str(self.repository / "script/upstream-sync/resolve.sh"),
                *arguments,
            ],
            cwd=self.repository,
            env=self.environment(),
            text=True,
            capture_output=True,
            check=False,
        )

    def setup(self) -> None:
        result = self.resolve("setup", str(self.artifact), str(self.work))
        assert result.returncode == 0, result.stdout + result.stderr

    def sandbox(self, command: str) -> subprocess.CompletedProcess[str]:
        environment = self.environment()
        environment["SYNC_WORK"] = str(self.work)
        return subprocess.run(
            [str(self.repository / "script/upstream-sync/sandbox-exec"), command],
            env=environment,
            text=True,
            capture_output=True,
            check=False,
        )

    @property
    def candidate(self) -> Path:
        return self.work / "candidate"

    def agent(self, files: dict[str, str], tests: str | None = None) -> None:
        for name, contents in files.items():
            path = self.candidate / name
            path.parent.mkdir(parents=True, exist_ok=True)
            _ = path.write_text(contents)
            _ = git(self.candidate, "add", "--", name)
        _ = (self.work / "out/resolution.md").write_text(
            "Kept the fork's value and adopted upstream's label.\n"
        )
        if tests is not None:
            _ = (self.work / "out/tests.txt").write_text(tests)

    def finish(self) -> str:
        _ = self.resolve("validate", str(self.work))
        packaged = self.resolve(
            "package", str(self.work), str(self.output), str(self.artifact)
        )
        assert packaged.returncode == 0, packaged.stdout + packaged.stderr
        return (self.output / "result").read_text().strip()


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


@require_sandbox
class ResolverTests(unittest.TestCase):
    def fixture(self, factory: Callable[[Path], Fixture] = conflict_fixture) -> Fixture:
        root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        fixture = factory(root)
        fixture.setup()
        return fixture

    def assert_failed(self, fixture: Fixture, check: str) -> None:
        self.assertEqual(fixture.finish(), "conflict")
        self.assertFalse((fixture.output / "sync.bundle").exists())
        body = (fixture.output / "issue-body.md").read_text()
        self.assertIn(f"Validation failed at **{check}**", body)
        self.assertIn("alpha/src/lib.rs", body)
        self.assertLessEqual(len(body.encode()), 60000)

    def test_setup_rebuilds_the_exact_recorded_merge(self):
        fixture = self.fixture()
        shas = (fixture.work / "in/shas").read_text()
        self.assertIn(f"fork={fixture.fork}", shas)
        self.assertIn(f"upstream={fixture.upstream}", shas)
        self.assertIn(f"base={fixture.base}", shas)
        self.assertEqual(git(fixture.candidate, "rev-parse", "HEAD"), fixture.fork)
        self.assertEqual(
            git(fixture.candidate, "rev-parse", "MERGE_HEAD"), fixture.upstream
        )
        self.assertEqual(
            git(fixture.candidate, "diff", "--name-only", "--diff-filter=U"),
            "alpha/src/lib.rs",
        )
        self.assertIn("<<<<<<<", (fixture.candidate / "alpha/src/lib.rs").read_text())
        stages = git(fixture.candidate, "ls-files", "--stage", "--", "alpha/src/lib.rs")
        self.assertEqual(
            sorted(line.split()[2] for line in stages.splitlines()), ["1", "2", "3"]
        )
        prompt = (fixture.work / "prompt.md").read_text()
        for sha in (fixture.fork, fixture.upstream, fixture.base):
            self.assertIn(sha, prompt)
        self.assertIn(
            f"{fixture.repository}/.agents/skills/upstream-sync-conflict/SKILL.md",
            prompt,
        )

    def test_setup_uses_the_recorded_upstream_even_after_upstream_moves(self):
        root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        fixture = conflict_fixture(root)
        _ = git(fixture.repository, "switch", "-q", "upstream")
        fixture.write({"later.txt": "after the artifact\n"})
        fixture.commit("later upstream commit")
        _ = git(fixture.repository, "switch", "-q", "master")
        fixture.setup()
        self.assertEqual(
            git(fixture.candidate, "rev-parse", "MERGE_HEAD"), fixture.upstream
        )
        self.assertFalse((fixture.candidate / "later.txt").exists())

    def test_setup_refuses_a_candidate_for_another_fork_commit(self):
        root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        fixture = conflict_fixture(root)
        fixture.write({"moved.txt": "master moved\n"})
        fixture.commit("master moved")
        result = fixture.resolve("setup", str(fixture.artifact), str(fixture.work))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("not the trusted fork commit", result.stderr)

    def test_successful_resolution_publishes_a_merge_with_upstream_ancestry(self):
        fixture = self.fixture()
        fixture.agent(
            {"alpha/src/lib.rs": RESOLVED_ALPHA},
            tests="alpha\ttest(value_is_positive)\n",
        )
        self.assertEqual(fixture.finish(), "resolved")
        _ = git(
            fixture.repository,
            "fetch",
            "-q",
            str(fixture.output / "sync.bundle"),
            "sync/upstream:refs/check/resolved",
        )
        head = "refs/check/resolved"
        self.assertEqual(
            git(fixture.repository, "rev-parse", f"{head}^1"), fixture.fork
        )
        self.assertEqual(
            git(fixture.repository, "rev-parse", f"{head}^2"), fixture.upstream
        )
        self.assertEqual(
            git(fixture.repository, "log", "-1", "--format=%an", head),
            "github-actions[bot]",
        )
        merged = git(fixture.repository, "show", f"{head}:alpha/src/lib.rs")
        self.assertNotIn("<<<<<<<", merged)
        self.assertIn('"upstream"', merged)
        self.assertEqual(
            git(
                fixture.repository,
                "diff",
                "--name-only",
                fixture.fork,
                head,
                "--",
                ".github",
            ),
            "",
        )
        self.assertIn(
            "Kept the fork's value", (fixture.output / "resolution.md").read_text()
        )
        for name in LISTS:
            self.assertTrue((fixture.output / name).is_file(), name)

    def test_agent_that_does_nothing_produces_no_candidate(self):
        self.assert_failed(self.fixture(), "unmerged paths")

    def test_staged_conflict_markers_produce_no_candidate(self):
        fixture = self.fixture()
        _ = git(fixture.candidate, "add", "--", "alpha/src/lib.rs")
        self.assert_failed(fixture, "conflict markers")

    def test_compile_failure_produces_no_candidate(self):
        fixture = self.fixture()
        fixture.agent({
            "alpha/src/lib.rs": RESOLVED_ALPHA.replace("    2\n", '    "two"\n')
        })
        self.assert_failed(fixture, "workspace compile")

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
        self.assert_failed(fixture, "workspace compile")
        self.assertIn("fork_tool", (fixture.output / "issue-body.md").read_text())

    def test_failing_crate_test_produces_no_candidate(self):
        fixture = self.fixture()
        fixture.agent({
            "alpha/src/lib.rs": RESOLVED_ALPHA.replace(
                "assert!(super::value() > 0)", "assert!(super::value() > 9)"
            )
        })
        self.assert_failed(fixture, "tests")

    def test_failing_targeted_test_produces_no_candidate(self):
        fixture = self.fixture()
        fixture.agent(
            {"alpha/src/lib.rs": RESOLVED_ALPHA},
            tests="alpha\ttest(no_such_test_exists)\n",
        )
        self.assertEqual(fixture.finish(), "conflict")
        self.assertIn(
            "Validation failed at **targeted tests**",
            (fixture.output / "issue-body.md").read_text(),
        )

    def test_github_changes_produce_no_candidate(self):
        fixture = self.fixture()
        fixture.agent({
            "alpha/src/lib.rs": RESOLVED_ALPHA,
            ".github/workflows/ci.yml": "changed by the agent\n",
        })
        self.assert_failed(fixture, "automation unchanged")

    def test_changes_after_validation_produce_no_candidate(self):
        fixture = self.fixture()
        fixture.agent({"alpha/src/lib.rs": RESOLVED_ALPHA})
        _ = fixture.resolve("validate", str(fixture.work))
        fixture.agent({"alpha/src/lib.rs": RESOLVED_ALPHA + "\npub fn late() {}\n"})
        packaged = fixture.resolve(
            "package", str(fixture.work), str(fixture.output), str(fixture.artifact)
        )
        self.assertEqual(packaged.returncode, 0, packaged.stderr)
        self.assertEqual((fixture.output / "result").read_text(), "conflict\n")
        self.assertIn(
            "changed after validation", (fixture.output / "issue-body.md").read_text()
        )

    def test_failed_setup_still_reports_the_conflict(self):
        root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        fixture = conflict_fixture(root)
        packaged = fixture.resolve(
            "package", str(fixture.work), str(fixture.output), str(fixture.artifact)
        )
        self.assertEqual(packaged.returncode, 0, packaged.stderr)
        self.assertEqual((fixture.output / "result").read_text(), "conflict\n")
        body = (fixture.output / "issue-body.md").read_text()
        self.assertIn("could not rebuild the merge", body)
        self.assertIn("alpha/src/lib.rs", body)


@require_sandbox
class SandboxTests(unittest.TestCase):
    directory: ClassVar[tempfile.TemporaryDirectory[str]]
    fixture: ClassVar[Fixture]

    @override
    @classmethod
    def setUpClass(cls):
        cls.directory = tempfile.TemporaryDirectory()
        cls.fixture = conflict_fixture(Path(cls.directory.name))
        cls.fixture.setup()

    @override
    @classmethod
    def tearDownClass(cls):
        cls.directory.cleanup()

    def hook(self, event: dict[str, object]) -> HookDecision:
        result = subprocess.run(
            [
                str(self.fixture.repository / "script/upstream-sync/sandbox-hook"),
                str(self.fixture.work),
            ],
            input=json.dumps(event),
            text=True,
            capture_output=True,
            check=True,
        )
        return cast(HookOutput, json.loads(result.stdout))["hookSpecificOutput"]

    def test_candidate_commands_see_no_credentials(self):
        holder = subprocess.Popen(["sleep", "60"], env={**os.environ, **CANARIES})
        self.addCleanup(holder.kill)
        result = self.fixture.sandbox(
            "env; for f in /proc/[0-9]*/environ; do tr '\\0' '\\n' <\"$f\" 2>/dev/null; done; ls /proc"
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        for value in CANARIES.values():
            self.assertNotIn(value, result.stdout)
            self.assertNotIn(value, result.stderr)
        self.assertNotIn(str(holder.pid), result.stdout.split())

    def test_hook_routes_claude_bash_through_the_sandbox(self):
        output = self.hook({
            "tool_name": "Bash",
            "tool_input": {"command": "env", "description": "show env"},
        })
        self.assertEqual(output["permissionDecision"], "allow")
        updated = output.get("updatedInput", {})
        self.assertEqual(updated.get("description"), "show env")
        result = subprocess.run(
            ["bash", "-c", updated["command"]],
            env=self.fixture.environment(),
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("CI=true", result.stdout)
        for value in CANARIES.values():
            self.assertNotIn(value, result.stdout)

    def test_hook_denies_bash_input_without_a_command(self):
        output = self.hook({"tool_name": "Bash", "tool_input": {}})
        self.assertEqual(output["permissionDecision"], "deny")

    def test_home_and_trusted_checkout_are_hidden(self):
        secret = self.fixture.home / ".config/secret"
        secret.parent.mkdir(parents=True, exist_ok=True)
        _ = secret.write_text("canary-home-secret\n")
        result = self.fixture.sandbox(
            f"cat {secret}; cat {self.fixture.repository}/.github/workflows/ci.yml; ls {self.fixture.home}"
        )
        self.assertNotIn("canary-home-secret", result.stdout)
        self.assertNotIn("trusted workflow", result.stdout)

    def test_trusted_paths_are_not_writable(self):
        targets = [
            self.fixture.repository / "planted",
            self.fixture.work / "sandbox/planted",
            self.fixture.work / "in/planted",
            self.fixture.candidate / ".git/hooks/pre-commit",
        ]
        _ = self.fixture.sandbox("; ".join(f"echo x > {target}" for target in targets))
        for target in targets:
            self.assertFalse(target.exists(), target)

    def test_unix_sockets_such_as_docker_are_unreachable(self):
        path = Path(self.directory.name) / "docker.sock"
        server = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        self.addCleanup(server.close)
        server.bind(str(path))
        server.listen(1)
        probe = textwrap.dedent(
            f"""
            python3 - <<'PY'
            import socket
            for path in ("{path}", "/var/run/docker.sock", "/run/docker.sock"):
                try:
                    client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                    client.connect(path)
                    print("connected", path)
                except OSError as error:
                    print("refused", path, error.errno)
            PY
            """
        )
        result = self.fixture.sandbox(probe)
        self.assertNotIn("connected", result.stdout, result.stdout + result.stderr)

    def test_claude_settings_protect_trusted_files_and_route_bash(self):
        work = self.fixture.work
        settings = cast(
            ClaudeSettings,
            json.loads((work / "sandbox/claude-settings.json").read_text()),
        )
        deny = settings["permissions"]["deny"]
        protected = (
            self.fixture.repository,
            Path(os.path.realpath(self.fixture.home)) / ".claude",
            work / "sandbox",
            work / "in",
        )
        for path in protected:
            for tool in ("Edit", "Write"):
                self.assertIn(f"{tool}(/{path}/**)", deny)
        self.assertIn("Monitor", deny)
        hooks = settings["hooks"]["PreToolUse"]
        self.assertEqual(len(hooks), 1)
        self.assertEqual(hooks[0]["matcher"], "Bash")
        self.assertEqual(
            hooks[0]["hooks"][0]["command"],
            f"{self.fixture.repository}/script/upstream-sync/sandbox-hook {work}",
        )
        self.assertFalse(settings["disableAllHooks"])
        self.assertFalse(settings["enableAllProjectMcpServers"])

    def test_the_mise_toolchain_is_available_inside(self):
        result = self.fixture.sandbox(
            "cargo --version && cargo nextest --version && dprint --version"
        )
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("dprint 0.58.0", result.stdout)


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
        resolve = self.job("resolve")
        self.assertIn("if: needs.prepare.outputs.result == 'partial'", resolve)
        self.assertEqual(self.workflow.count("claude-code-action"), 1)

    def test_resolver_has_no_github_write_credential(self):
        resolve = self.job("resolve")
        self.assertIn("permissions: { contents: read }", resolve)
        self.assertNotIn("SYNC_TOKEN", resolve)
        self.assertNotIn("github.token", resolve)
        self.assertNotIn("id-token", self.workflow)
        self.assertNotIn("persist-credentials: true", resolve)
        self.assertEqual(resolve.count("secrets."), 1)
        self.assertIn(
            "claude_code_oauth_token: ${{ secrets.CLAUDE_CODE_OAUTH_TOKEN }}", resolve
        )

    def test_claude_uses_only_trusted_configuration(self):
        resolve = self.job("resolve")
        self.assertIn(
            "anthropics/claude-code-action/base-action@97c53473391bff1901034d4b454b5bac7ab7a029",
            resolve,
        )
        self.assertIn("--setting-sources user", resolve)
        self.assertIn("--model claude-opus-5-5", resolve)
        self.assertIn("--permission-mode auto", resolve)
        self.assertIn("--add-dir /mnt/sync/candidate", resolve)
        self.assertNotIn("working-directory", resolve)
        self.assertNotIn("CLAUDE_WORKING_DIR", resolve)
        self.assertNotIn("CLAUDE_CODE_ADDITIONAL_DIRECTORIES_CLAUDE_MD", resolve)

    def test_mise_provisions_the_resolver(self):
        resolve = self.job("resolve")
        self.assertIn(
            "jdx/mise-action@7a4e45a543138629540c9a1616d08632b893e492", resolve
        )
        self.assertIn("bootstrap: true, bootstrap_skip: compose", resolve)
        for unmanaged in (
            "setup-node",
            "setup-python",
            "rust-toolchain",
            "rustup ",
            "npm install",
            "pip install",
        ):
            self.assertNotIn(unmanaged, resolve)


if __name__ == "__main__":
    _ = unittest.main()
