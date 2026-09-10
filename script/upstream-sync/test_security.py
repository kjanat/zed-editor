import importlib.util
import os
import subprocess
import tempfile
import textwrap
import time
import unittest
from functools import cached_property
from itertools import takewhile
from pathlib import Path
from typing import Protocol, cast
from unittest.mock import patch


class Publisher(Protocol):
    REPOSITORY: str

    def run(self, *arguments: str, cwd: Path | None = None) -> str: ...
    def validate(self, candidate: Path, base: str) -> str: ...
    def report(self, title: str, body: str) -> None: ...
    def inspect_sync_pr(self) -> str: ...
    def resolution_details(self, directory: Path) -> str: ...
    def sync_pr_body(self, resolutions: str = "") -> str: ...
    def publish(self, directory: Path) -> None: ...


class Exporter:
    @cached_property
    def script(self) -> str:
        workflow = (
            Path(__file__).resolve().parents[2]
            / ".github/workflows/fork_upstream_sync.yml"
        ).read_text()
        step = workflow.split("      - name: Export only regular candidate files\n", 1)[
            1
        ]
        block = step.split("        run: |\n", 1)[1]
        return textwrap.dedent(
            "".join(
                takewhile(
                    lambda line: not line.strip() or line.startswith("          "),
                    block.splitlines(keepends=True),
                )
            )
        )

    def export(self, source: Path, destination: Path) -> None:
        with patch.dict(
            os.environ, {"sync_output": str(source), "sync_export": str(destination)}
        ):
            exec(compile(self.script, "fork_upstream_sync.yml:export", "exec"), {})


def load(name: str) -> object:
    spec = importlib.util.spec_from_file_location(
        name, Path(__file__).with_name(f"{name}.py")
    )
    if spec is None or spec.loader is None:
        raise ImportError(f"Cannot load {name}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


exporter = Exporter()
publisher = cast(Publisher, load("publish"))


class ExportTests(unittest.TestCase):
    def test_rejects_symlinks_without_copying_the_target(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source"
            source.mkdir()
            _ = (root / "private").write_text("synthetic canary")
            (source / "result").symlink_to(root / "private")
            with self.assertRaises(OSError):
                exporter.export(source, root / "export")
            self.assertFalse((root / "export" / "result").exists())

    def test_exports_only_known_regular_files(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source"
            source.mkdir()
            _ = (source / "result").write_text("conflict\n")
            _ = (source / "issue-body.md").write_text("Conflict details")
            _ = (source / "ignored").write_text("not an artifact")
            exporter.export(source, root / "export")
            self.assertEqual(
                sorted(path.name for path in (root / "export").iterdir()),
                ["issue-body.md", "result"],
            )

    def test_rejects_fifo_without_blocking(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source"
            source.mkdir()
            os.mkfifo(source / "result")
            with self.assertRaises(ValueError):
                exporter.export(source, root / "export")


class CandidateFixture:
    def __init__(self, root: Path):
        self.root: Path = root
        self.source: Path = self.root / "source"
        self.source.mkdir()
        _ = self.git("init", "-q", "-b", "master")
        _ = self.git("config", "user.name", "Test")
        _ = self.git("config", "user.email", "test@example.invalid")
        _ = self.git("config", "commit.gpgsign", "false")
        workflow = self.source / ".github" / "workflows" / "ci.yml"
        workflow.parent.mkdir(parents=True)
        _ = workflow.write_text("trusted workflow\n")
        _ = (self.source / "code").write_text("original\n")
        self.commit()
        self.base: str = self.git("rev-parse", "HEAD")
        self.destination: Path = self.root / "publisher"
        _ = subprocess.run(
            ["git", "clone", "-q", str(self.source), str(self.destination)], check=True
        )
        _ = self.git("switch", "-q", "-c", "sync/upstream")

    def git(self, *arguments: str) -> str:
        return subprocess.check_output(
            ["git", *arguments], cwd=self.source, text=True
        ).strip()

    def commit(self):
        _ = self.git("add", "--all")
        _ = self.git("commit", "-q", "-m", "fixture")

    def validate(self):
        bundle = self.root / "candidate.bundle"
        _ = self.git(
            "bundle", "create", str(bundle), "refs/heads/sync/upstream", "^master"
        )
        original_run = publisher.run

        def run_in_destination(*arguments: str) -> str:
            return original_run(*arguments, cwd=self.destination)

        with patch.object(publisher, "run", side_effect=run_in_destination):
            return publisher.validate(bundle, self.base)


class CandidateTests(unittest.TestCase):
    @cached_property
    def fixture(self) -> CandidateFixture:
        root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        return CandidateFixture(root)

    def test_accepts_code_without_executing_or_checking_it_out(self):
        _ = (self.fixture.source / "code").write_text("untrusted candidate\n")
        self.fixture.commit()
        self.assertEqual(self.fixture.validate(), self.fixture.git("rev-parse", "HEAD"))
        self.assertEqual((self.fixture.destination / "code").read_text(), "original\n")

    def test_rejects_added_workflow(self):
        _ = (self.fixture.source / ".github/workflows/steal.yml").write_text(
            "malicious workflow\n"
        )
        self.fixture.commit()
        with self.assertRaisesRegex(ValueError, "changes .github"):
            _ = self.fixture.validate()

    def test_rejects_modified_workflow(self):
        _ = (self.fixture.source / ".github/workflows/ci.yml").write_text(
            "modified workflow\n"
        )
        self.fixture.commit()
        with self.assertRaisesRegex(ValueError, "changes .github"):
            _ = self.fixture.validate()

    def test_rejects_deleted_workflow(self):
        (self.fixture.source / ".github/workflows/ci.yml").unlink()
        self.fixture.commit()
        with self.assertRaisesRegex(ValueError, "changes .github"):
            _ = self.fixture.validate()

    def test_rejects_replaced_github_directory(self):
        (self.fixture.source / ".github/workflows/ci.yml").unlink()
        (self.fixture.source / ".github/workflows").rmdir()
        (self.fixture.source / ".github").rmdir()
        (self.fixture.source / ".github").symlink_to("elsewhere")
        self.fixture.commit()
        with self.assertRaisesRegex(ValueError, "changes .github"):
            _ = self.fixture.validate()

    def test_rejects_unrelated_history(self):
        _ = self.fixture.git("switch", "--orphan", "unrelated")
        _ = (self.fixture.source / "other").write_text("unrelated\n")
        self.fixture.commit()
        _ = self.fixture.git("branch", "-f", "sync/upstream", "HEAD")
        with self.assertRaises(subprocess.CalledProcessError):
            _ = self.fixture.validate()


class ReportingTests(unittest.TestCase):
    def test_only_conflict_reports_receive_conflict_label(self):
        for title in (
            "Upstream sync conflict",
            "Upstream sync needs attention",
            "Upstream sync requires security review",
        ):
            with (
                self.subTest(title=title),
                patch.object(publisher, "gh", side_effect=["[]", ""]) as gh,
            ):
                publisher.report(title, "details")
                arguments = gh.call_args.args
                self.assertEqual(arguments[:2], ("issue", "create"))
                self.assertEqual(
                    "upstream-sync-conflict" in arguments,
                    title == "Upstream sync conflict",
                )

    def test_successful_sync_does_not_close_mislabeled_incidents(self):
        import json

        issues = [
            {"number": 12, "title": "Upstream sync conflict"},
            {"number": 34, "title": "Upstream sync conflict (2026-09-09)"},
            {"number": 56, "title": "Upstream sync needs attention"},
            {"number": 78, "title": "Upstream sync requires security review"},
            {"number": 90, "title": "Upstream sync conflict investigation"},
        ]
        with patch.object(publisher, "gh", return_value=json.dumps(issues)):
            body = publisher.sync_pr_body()
        self.assertIn("Closes #12.", body)
        self.assertIn("Closes #34.", body)
        for number in (56, 78, 90):
            self.assertNotIn(f"Closes #{number}.", body)

    def test_reuses_current_and_legacy_conflict_titles(self):
        import json

        for title in ("Upstream sync conflict", "Upstream sync conflict (2026-09-09)"):
            with (
                self.subTest(title=title),
                patch.object(
                    publisher,
                    "gh",
                    side_effect=[
                        json.dumps([
                            {
                                "number": 1,
                                "title": "Upstream sync conflict investigation",
                            },
                            {"number": 42, "title": title},
                        ]),
                        "",
                        "",
                    ],
                ) as gh,
            ):
                publisher.report("Upstream sync conflict", "details")
                self.assertEqual(gh.call_args.args[:3], ("issue", "comment", "42"))
                self.assertEqual(
                    gh.call_args_list[-2].args,
                    ("issue", "edit", "42", "--add-label", "upstream-sync-conflict"),
                )

    def test_does_not_reuse_unrelated_conflict_title(self):
        import json

        with patch.object(
            publisher,
            "gh",
            side_effect=[
                json.dumps([
                    {"number": 1, "title": "Upstream sync conflict investigation"},
                ]),
                "",
            ],
        ) as gh:
            publisher.report("Upstream sync conflict", "details")
            self.assertEqual(gh.call_args.args[:2], ("issue", "create"))

    def test_resolution_lists_survive_export_and_appear_in_body(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source"
            source.mkdir()
            _ = (source / "result").write_text("resolved")
            for name, path in (
                ("formatting-only.txt", "src/a.rs"),
                ("formatted-three-way.txt", "src/<b>.rs"),
                ("lockfiles.txt", "Cargo.lock"),
            ):
                _ = (source / name).write_text(path + "\n")
            exporter.export(source, root / "export")
            details = publisher.resolution_details(root / "export")
            with patch.object(publisher, "gh", return_value="[]"):
                body = publisher.sync_pr_body(details)
                clean_body = publisher.sync_pr_body()
            for text in (
                "Formatting-only",
                "Formatted three-way",
                "Lockfile",
                "src/a.rs",
                "src/&lt;b&gt;.rs",
                "Cargo.lock",
            ):
                self.assertIn(text, body)
            self.assertNotIn("Resolved automatically", clean_body)
            self.assertTrue(body.endswith("Release Notes:\n\n- N/A\n"))

    def test_resolution_exports_reject_oversized_files_and_symlinks(self):
        for symlink in (False, True):
            with (
                self.subTest(symlink=symlink),
                tempfile.TemporaryDirectory() as temporary,
            ):
                root = Path(temporary)
                source = root / "source"
                source.mkdir()
                _ = (source / "result").write_text("resolved")
                metadata = source / "formatting-only.txt"
                if symlink:
                    metadata.symlink_to(root / "private")
                else:
                    _ = metadata.write_text("a" * 16001)
                with self.assertRaises((ValueError, OSError)):
                    exporter.export(source, root / "export")

    def test_failed_checks_alert_before_replacing_the_pr_head(self):
        import json

        details = {
            "mergeStateStatus": "CLEAN",
            "statusCheckRollup": [
                {"name": "tests", "conclusion": "FAILURE"},
                {"name": "style", "conclusion": "SUCCESS"},
                {"name": "pending", "conclusion": None},
            ],
        }
        with (
            patch.object(publisher, "gh", side_effect=["42", json.dumps(details)]),
            patch.object(publisher, "report") as report,
        ):
            self.assertEqual(publisher.inspect_sync_pr(), "42")
        self.assertEqual(report.call_args.args[0], "Upstream sync needs attention")
        self.assertIn("Failing checks: tests", cast(str, report.call_args.args[1]))

    def test_dirty_and_blocked_prs_alert_without_failed_checks(self):
        import json

        for state in ("DIRTY", "BLOCKED"):
            with (
                self.subTest(state=state),
                patch.object(
                    publisher,
                    "gh",
                    side_effect=[
                        "42",
                        json.dumps({
                            "mergeStateStatus": state,
                            "statusCheckRollup": [],
                        }),
                    ],
                ),
                patch.object(publisher, "report") as report,
            ):
                _ = publisher.inspect_sync_pr()
                report.assert_called_once()
                self.assertIn(state, cast(str, report.call_args.args[1]))

    def test_unknown_state_is_retried_without_false_alerts(self):
        import json

        with (
            patch.object(
                publisher,
                "gh",
                side_effect=[
                    "42",
                    json.dumps({
                        "mergeStateStatus": "UNKNOWN",
                        "statusCheckRollup": [],
                    }),
                    json.dumps({"mergeStateStatus": "CLEAN", "statusCheckRollup": []}),
                ],
            ),
            patch.object(time, "sleep") as sleep,
            patch.object(publisher, "report") as report,
        ):
            _ = publisher.inspect_sync_pr()
            sleep.assert_called_once_with(10)
            report.assert_not_called()

    def test_successful_body_closes_all_reported_conflicts_on_merge(self):
        with patch.object(
            publisher,
            "gh",
            return_value='[ {"number":12,"title":"Upstream sync conflict"}, {"number":34,"title":"Upstream sync conflict (2026-09-09)"} ]',
        ) as gh:
            body = publisher.sync_pr_body()
        self.assertIn("Closes #12.\nCloses #34.\n", body)
        self.assertTrue(body.endswith("Release Notes:\n\n- N/A\n"))
        self.assertEqual(gh.call_count, 1)
        self.assertEqual(gh.call_args.args[:2], ("issue", "list"))

    def test_no_conflicts_needs_no_closing_references(self):
        with patch.object(publisher, "gh", return_value="[]"):
            body = publisher.sync_pr_body()
        self.assertNotIn("Closes #", body)


class PublishFlowTests(unittest.TestCase):
    def test_incidents_remain_separate_through_successful_publication(self):
        import json

        for result in (
            "clean",
            "resolved",
            "conflict",
            "unchanged",
            "security",
            "foreign",
        ):
            for existing in ("", "42"):
                with (
                    self.subTest(result=result, existing=existing),
                    tempfile.TemporaryDirectory() as temporary,
                ):
                    directory = Path(temporary)
                    _ = (directory / "result").write_text(
                        "clean" if result in ("security", "foreign") else result
                    )
                    _ = (directory / "issue-body.md").write_text("Conflict details")
                    for name in (
                        "formatting-only.txt",
                        "formatted-three-way.txt",
                        "lockfiles.txt",
                    ):
                        _ = (directory / name).write_text("file.rs\n")
                    bodies: list[str] = []
                    calls: list[tuple[str, ...]] = []

                    def fake_gh(
                        *arguments: str,
                        calls: list[tuple[str, ...]] = calls,
                        bodies: list[str] = bodies,
                    ) -> str:
                        calls.append(arguments)
                        if arguments[:2] == ("issue", "list"):
                            return json.dumps([
                                {
                                    "number": 1,
                                    "title": "Upstream sync conflict (2026-09-09)",
                                },
                                {"number": 2, "title": "Upstream sync needs attention"},
                                {
                                    "number": 3,
                                    "title": "Upstream sync requires security review",
                                },
                            ])
                        if "--body-file" in arguments:
                            bodies.append(
                                Path(
                                    arguments[arguments.index("--body-file") + 1]
                                ).read_text()
                            )
                        return ""

                    head = "b" * 40
                    base = "a" * 40

                    def fake_git(
                        *arguments: str,
                        base: str = base,
                        head: str = head,
                        result: str = result,
                    ) -> str:
                        if arguments == ("rev-parse", "FETCH_HEAD"):
                            return base
                        if arguments == ("rev-parse", "refs/sync-previous"):
                            return head
                        if arguments[0] == "ls-remote":
                            return f"{head} refs/heads/sync/upstream"
                        if arguments[0] == "log":
                            return (
                                "Human"
                                if result == "foreign"
                                else "github-actions[bot]"
                            )
                        return ""

                    with (
                        patch.dict(
                            os.environ,
                            {
                                "GITHUB_REPOSITORY": publisher.REPOSITORY,
                                "GITHUB_SHA": base,
                            },
                        ),
                        patch.object(
                            publisher, "inspect_sync_pr", return_value=existing
                        ),
                        patch.object(
                            publisher,
                            "validate",
                            side_effect=ValueError("automation changed")
                            if result == "security"
                            else None,
                            return_value=head,
                        ),
                        patch.object(publisher, "git", side_effect=fake_git) as git,
                        patch.object(publisher, "gh", side_effect=fake_gh),
                        patch.object(publisher, "report") as report,
                    ):
                        if result == "security":
                            with self.assertRaises(ValueError):
                                publisher.publish(directory)
                        else:
                            publisher.publish(directory)
                        pushes = [
                            call for call in git.call_args_list if "push" in call.args
                        ]
                        self.assertEqual(
                            len(pushes), int(result in ("clean", "resolved"))
                        )
                        if result in ("clean", "resolved"):
                            report.assert_not_called()
                            self.assertEqual(
                                calls[-1][:2], ("pr", "edit" if existing else "create")
                            )
                            self.assertEqual(len(bodies), 1)
                            self.assertIn("Closes #1.", bodies[0])
                            self.assertNotIn("Closes #2.", bodies[0])
                            self.assertNotIn("Closes #3.", bodies[0])
                            self.assertEqual(
                                "Resolved automatically" in bodies[0],
                                result == "resolved",
                            )
                        elif result == "unchanged":
                            report.assert_not_called()
                            git.assert_not_called()
                        else:
                            expected = {
                                "conflict": "Upstream sync conflict",
                                "security": "Upstream sync requires security review",
                                "foreign": "Upstream sync needs attention",
                            }[result]
                            self.assertEqual(report.call_args.args[0], expected)
                            self.assertFalse(bodies)


if __name__ == "__main__":
    _ = unittest.main()
