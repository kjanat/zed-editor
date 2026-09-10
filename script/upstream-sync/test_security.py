import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch


def load(name):
    spec = importlib.util.spec_from_file_location(
        name, Path(__file__).with_name(f"{name}.py")
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


publisher = load("publish")
exporter = load("export")


class ExportTests(unittest.TestCase):
    def test_rejects_symlinks_without_copying_the_target(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source"
            source.mkdir()
            (root / "private").write_text("synthetic canary")
            (source / "result").symlink_to(root / "private")
            with self.assertRaises(OSError):
                exporter.export(source, root / "export")
            self.assertFalse((root / "export" / "result").exists())

    def test_exports_only_known_regular_files(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source"
            source.mkdir()
            (source / "result").write_text("conflict\n")
            (source / "issue-body.md").write_text("Conflict details")
            (source / "ignored").write_text("not an artifact")
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


class CandidateTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.source = self.root / "source"
        self.source.mkdir()
        self.git("init", "-q", "-b", "master")
        self.git("config", "user.name", "Test")
        self.git("config", "user.email", "test@example.invalid")
        self.git("config", "commit.gpgsign", "false")
        workflow = self.source / ".github" / "workflows" / "ci.yml"
        workflow.parent.mkdir(parents=True)
        workflow.write_text("trusted workflow\n")
        (self.source / "code").write_text("original\n")
        self.commit()
        self.base = self.git("rev-parse", "HEAD")
        self.destination = self.root / "publisher"
        subprocess.run(
            ["git", "clone", "-q", str(self.source), str(self.destination)], check=True
        )
        self.git("switch", "-q", "-c", "sync/upstream")

    def git(self, *arguments):
        return subprocess.check_output(
            ["git", *arguments], cwd=self.source, text=True
        ).strip()

    def commit(self):
        self.git("add", "--all")
        self.git("commit", "-q", "-m", "fixture")

    def validate(self):
        bundle = self.root / "candidate.bundle"
        self.git("bundle", "create", str(bundle), "refs/heads/sync/upstream", "^master")
        original_run = publisher.run
        with patch.object(
            publisher,
            "run",
            side_effect=lambda *args: original_run(*args, cwd=self.destination),
        ):
            return publisher.validate(bundle, self.base)

    def test_accepts_code_without_executing_or_checking_it_out(self):
        (self.source / "code").write_text("untrusted candidate\n")
        self.commit()
        self.assertEqual(self.validate(), self.git("rev-parse", "HEAD"))
        self.assertEqual((self.destination / "code").read_text(), "original\n")

    def test_rejects_added_workflow(self):
        (self.source / ".github/workflows/steal.yml").write_text("malicious workflow\n")
        self.commit()
        with self.assertRaisesRegex(ValueError, "changes .github"):
            self.validate()

    def test_rejects_modified_workflow(self):
        (self.source / ".github/workflows/ci.yml").write_text("modified workflow\n")
        self.commit()
        with self.assertRaisesRegex(ValueError, "changes .github"):
            self.validate()

    def test_rejects_deleted_workflow(self):
        (self.source / ".github/workflows/ci.yml").unlink()
        self.commit()
        with self.assertRaisesRegex(ValueError, "changes .github"):
            self.validate()

    def test_rejects_replaced_github_directory(self):
        (self.source / ".github/workflows/ci.yml").unlink()
        (self.source / ".github/workflows").rmdir()
        (self.source / ".github").rmdir()
        (self.source / ".github").symlink_to("elsewhere")
        self.commit()
        with self.assertRaisesRegex(ValueError, "changes .github"):
            self.validate()

    def test_rejects_unrelated_history(self):
        self.git("switch", "--orphan", "unrelated")
        (self.source / "other").write_text("unrelated\n")
        self.commit()
        self.git("branch", "-f", "sync/upstream", "HEAD")
        with self.assertRaises(subprocess.CalledProcessError):
            self.validate()


class ReportingTests(unittest.TestCase):
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
        self.assertIn("Failing checks: tests", report.call_args.args[1])

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
                publisher.inspect_sync_pr()
                report.assert_called_once()
                self.assertIn(state, report.call_args.args[1])

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
            patch.object(publisher.time, "sleep") as sleep,
            patch.object(publisher, "report") as report,
        ):
            publisher.inspect_sync_pr()
            sleep.assert_called_once_with(10)
            report.assert_not_called()

    def test_successful_body_closes_all_reported_conflicts_on_merge(self):
        with patch.object(publisher, "gh", return_value="12\n34") as gh:
            body = publisher.sync_pr_body()
        self.assertIn("Closes #12.\nCloses #34.\n", body)
        self.assertTrue(body.endswith("Release Notes:\n\n- N/A\n"))
        self.assertEqual(gh.call_count, 1)
        self.assertEqual(gh.call_args.args[:2], ("issue", "list"))

    def test_no_conflicts_needs_no_closing_references(self):
        with patch.object(publisher, "gh", return_value=""):
            body = publisher.sync_pr_body()
        self.assertNotIn("Closes #", body)


if __name__ == "__main__":
    unittest.main()
