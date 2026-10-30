"""Exercise the once-per-head sync PR gate and the verified update push against local remotes."""

import importlib.util
import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path
from types import ModuleType
from unittest.mock import MagicMock, patch

ISSUE = "https://github.com/kjanat/zed-editor/issues/9"


def load() -> ModuleType:
    spec = importlib.util.spec_from_file_location(
        "repair_sync_pr", Path(__file__).with_name("repair-sync-pr.py")
    )
    if spec is None or spec.loader is None:
        raise ImportError("Cannot load repair-sync-pr.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


repair = load()


def git(directory: Path, *arguments: str) -> str:
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
        check=True,
        text=True,
        capture_output=True,
    ).stdout.strip()


class SyncPrUpdateTests(unittest.TestCase):
    def setUp(self):
        root = Path(self.enterContext(tempfile.TemporaryDirectory()))
        self.origin: Path = root / "origin.git"
        self.zed: Path = root / "zed.git"
        self.work: Path = root / "work"
        self.directory: Path = root / "artifact"
        self.outputs: Path = root / "outputs"
        for bare in (self.origin, self.zed):
            _ = git(root, "init", "-q", "--bare", "-b", "master", str(bare))
        _ = git(root, "init", "-q", "-b", "master", str(self.work))
        _ = git(self.work, "config", "user.name", "Test")
        _ = git(self.work, "config", "user.email", "test@example.invalid")
        _ = git(self.work, "remote", "add", "origin", str(self.origin))
        _ = git(self.work, "remote", "add", "zed", str(self.zed))
        self.write("fork.txt", "fork\n")
        self.write(".github/workflows/ci.yml", "trusted\n")
        self.master: str = self.commit("baseline")
        _ = git(self.work, "switch", "-q", "-c", "upstream")
        self.write("upstream.txt", "upstream\n")
        self.upstream: str = self.commit("upstream change")
        _ = git(self.work, "switch", "-q", "-c", "sync/upstream", "master")
        _ = git(self.work, "merge", "-q", "--no-ff", "upstream", "-m", "Merge upstream")
        self.head: str = git(self.work, "rev-parse", "HEAD")
        _ = git(self.work, "push", "-q", "origin", "master", "sync/upstream")
        _ = git(self.work, "push", "-q", "zed", "upstream:main")
        self.directory.mkdir()
        self.outputs.touch()
        previous = Path.cwd()
        os.chdir(self.work)
        self.addCleanup(os.chdir, previous)
        for patcher in (
            patch.dict(
                os.environ,
                {
                    "GITHUB_REPOSITORY": "kjanat/zed-editor",
                    "GITHUB_OUTPUT": str(self.outputs),
                    "HEAD_BRANCH": "sync/upstream",
                    "HEAD_SHA": self.head,
                    "UPSTREAM": "",
                },
            ),
            patch.object(repair, "UPSTREAM", str(self.zed)),
            patch("time.sleep"),
            patch("builtins.print"),
        ):
            _ = patcher.start()
            self.addCleanup(patcher.stop)
        _ = os.environ.pop("GITHUB_STEP_SUMMARY", None)

    def write(self, name: str, contents: str):
        path = self.work / name
        path.parent.mkdir(parents=True, exist_ok=True)
        _ = path.write_text(contents)

    def commit(self, message: str) -> str:
        _ = git(self.work, "add", "-A")
        _ = git(self.work, "commit", "-q", "-m", message)
        return git(self.work, "rev-parse", "HEAD")

    def output(self) -> dict[str, str]:
        return dict(
            line.split("=", 1) for line in self.outputs.read_text().splitlines()
        )

    def tip(self) -> str:
        return git(self.origin, "rev-parse", "refs/heads/sync/upstream")

    def advance_upstream(self) -> str:
        _ = git(self.work, "switch", "-q", "upstream")
        self.write("upstream.txt", "upstream moved\n")
        moved = self.commit("upstream moves")
        _ = git(self.work, "push", "-q", "zed", "upstream:main")
        _ = git(self.work, "switch", "-q", "sync/upstream")
        return moved

    def gate(
        self,
        pull: str = "7",
        branch: str = "sync/upstream",
        runs: list[dict[str, object]] | None = None,
        issues: str = "",
        log: str = "failed job log",
    ) -> tuple[MagicMock, MagicMock]:
        def gh(*arguments: str) -> str:
            if arguments[:2] == ("pr", "view"):
                return json.dumps({"headRefName": branch, "headRefOid": self.head})
            if arguments[:2] == ("run", "list"):
                self.assertIn(self.head, arguments)
                return json.dumps(
                    runs
                    if runs is not None
                    else [
                        {
                            "databaseId": 41,
                            "status": "completed",
                            "conclusion": "success",
                        }
                    ]
                )
            if arguments[:2] == ("issue", "list"):
                return issues
            if arguments[:2] == ("run", "view"):
                self.assertEqual(arguments, ("run", "view", "41", "--log-failed"))
                return log
            raise AssertionError(f"unexpected gh {arguments}")

        with (
            patch.object(repair.sync_pr, "any_open_sync_pr", return_value=pull),
            patch.object(repair.sync_pr, "gh", side_effect=gh) as called,
            patch.object(repair.sync_pr, "report", return_value=ISSUE) as report,
        ):
            self.assertTrue(repair.gate(self.directory))
        return called, report

    def failed(self) -> list[dict[str, object]]:
        return [{"databaseId": 41, "status": "completed", "conclusion": "failure"}]

    def update(
        self,
        files: dict[str, str],
        parents: tuple[str, ...] | None = None,
        message: str | None = None,
    ) -> str:
        parents = parents or (self.head,)
        _ = git(self.work, "switch", "-q", "--detach", parents[0])
        for name, contents in files.items():
            self.write(name, contents)
        _ = git(self.work, "add", "-A")
        tree = git(self.work, "write-tree")
        arguments = [argument for parent in parents for argument in ("-p", parent)]
        commit = git(
            self.work,
            "commit-tree",
            tree,
            *arguments,
            "-m",
            message or f"Update the sync PR\n\nSync-Update: {self.head}",
        )
        _ = git(self.work, "update-ref", "refs/sync/repaired", commit)
        _ = git(
            self.work,
            "bundle",
            "create",
            str(self.directory / "repair.bundle"),
            "refs/sync/repaired",
            f"^{self.head}",
        )
        _ = (self.directory / "result").write_text("repaired\n")
        _ = git(self.work, "switch", "-q", "--discard-changes", "sync/upstream")
        return commit

    def push(self) -> tuple[bool, MagicMock]:
        with (
            patch.object(repair.sync_pr, "report", return_value=ISSUE) as report,
            patch.object(repair.sync_pr, "open_pr_from", return_value="7"),
        ):
            pushed = repair.push(self.directory)
        return pushed, report

    def test_no_open_sync_pr_lets_prepare_run(self):
        called, report = self.gate(pull="")
        self.assertEqual(self.output(), {"open": "false", "run": "false"})
        called.assert_not_called()
        report.assert_not_called()

    def test_running_ci_waits_for_the_next_sync(self):
        _, report = self.gate(
            runs=[{"databaseId": 41, "status": "in_progress", "conclusion": ""}]
        )
        output = self.output()
        self.assertEqual((output["open"], output["run"]), ("true", "false"))
        report.assert_not_called()

    def test_a_green_and_current_pr_needs_nothing(self):
        _, report = self.gate()
        output = self.output()
        self.assertEqual(
            output,
            {
                "open": "true",
                "head": self.head,
                "branch": "sync/upstream",
                "run": "false",
            },
        )
        report.assert_not_called()

    def test_failed_ci_hands_the_pr_to_claude_with_its_log(self):
        _, report = self.gate(runs=self.failed())
        output = self.output()
        self.assertEqual((output["run"], output["upstream"]), ("true", ""))
        title, body = report.call_args.args
        self.assertEqual(title, "Upstream sync needs attention")
        self.assertIn("fix the failed CI run", body)
        self.assertIn(
            f"<!-- claude-update head={self.head} upstream={self.upstream} -->", body
        )
        self.assertEqual((self.directory / "failed.log").read_text(), "failed job log")

    def test_new_upstream_commits_are_handed_to_claude(self):
        moved = self.advance_upstream()
        _, report = self.gate()
        output = self.output()
        self.assertEqual((output["run"], output["upstream"]), ("true", moved))
        self.assertIn(f"merge upstream {moved}", report.call_args.args[1])
        self.assertFalse((self.directory / "failed.log").exists())

    def test_a_failure_and_new_upstream_go_to_one_attempt(self):
        moved = self.advance_upstream()
        _, report = self.gate(runs=self.failed())
        self.assertEqual(self.output()["upstream"], moved)
        self.assertIn(
            f"fix the failed CI run and merge upstream {moved}",
            report.call_args.args[1],
        )
        self.assertTrue((self.directory / "failed.log").exists())

    def test_a_state_claude_already_tried_is_left_alone(self):
        called, report = self.gate(
            runs=self.failed(),
            issues=f"<!-- claude-update head={self.head} upstream={self.upstream} -->\n",
        )
        self.assertEqual(self.output()["run"], "false")
        report.assert_not_called()
        self.assertNotIn(
            ("run", "view"), [call.args[:2] for call in called.call_args_list]
        )

    def test_new_upstream_commits_allow_another_attempt(self):
        moved = self.advance_upstream()
        _, report = self.gate(
            runs=self.failed(),
            issues=f"<!-- claude-update head={self.head} upstream={self.upstream} -->\n",
        )
        self.assertEqual(self.output()["upstream"], moved)
        self.assertIn(
            f"<!-- claude-update head={self.head} upstream={moved} -->",
            report.call_args.args[1],
        )

    def test_an_attempt_on_another_head_does_not_count(self):
        _ = self.gate(
            runs=self.failed(),
            issues="<!-- claude-update head="
            + "0" * 40
            + f" upstream={self.upstream} -->\n",
        )
        self.assertEqual(self.output()["run"], "true")

    def test_the_saved_log_keeps_its_last_bytes(self):
        _ = self.gate(runs=self.failed(), log="x" * repair.LOG_LIMIT + "end")
        log = (self.directory / "failed.log").read_bytes()
        self.assertEqual(len(log), repair.LOG_LIMIT)
        self.assertTrue(log.endswith(b"end"))

    def test_only_sync_branches_are_accepted(self):
        with self.assertRaisesRegex(ValueError, "Not a sync branch"):
            _ = self.gate(branch="feature")

    def test_a_verified_fix_is_pushed_to_the_pr_branch(self):
        commit = self.update({"fork.txt": "fixed\n"})
        pushed, report = self.push()
        self.assertTrue(pushed)
        report.assert_not_called()
        self.assertEqual(self.tip(), commit)

    def test_a_verified_upstream_merge_is_pushed_to_the_pr_branch(self):
        moved = self.advance_upstream()
        os.environ["UPSTREAM"] = moved
        commit = self.update(
            {"upstream.txt": "upstream moved\n"}, parents=(self.head, moved)
        )
        pushed, _ = self.push()
        self.assertTrue(pushed)
        self.assertEqual(self.tip(), commit)

    def test_an_update_without_the_upstream_parent_is_refused(self):
        os.environ["UPSTREAM"] = self.advance_upstream()
        _ = self.update({"fork.txt": "fixed\n"})
        with self.assertRaisesRegex(ValueError, "exactly"):
            _ = self.push()
        self.assertEqual(self.tip(), self.head)

    def test_an_update_on_another_parent_is_refused(self):
        _ = self.update({"fork.txt": "fixed\n"}, parents=(self.master,))
        with self.assertRaisesRegex(ValueError, "exactly"):
            _ = self.push()
        self.assertEqual(self.tip(), self.head)

    def push_with_auto_merge(
        self,
        auto_merge: dict[str, str] | None = None,
        heads: list[str] | None = None,
    ):
        calls: list[tuple[str, ...]] = []
        shown = iter(heads or [])

        def gh(*arguments: str) -> str:
            calls.append(arguments)
            if arguments[:2] == ("pr", "view") and "autoMergeRequest" in arguments:
                return json.dumps({"autoMergeRequest": auto_merge})
            if arguments[:2] == ("pr", "view"):
                return json.dumps({
                    "headRefName": "sync/upstream",
                    "headRefOid": next(shown),
                })
            if arguments[:2] == ("pr", "merge"):
                return ""
            raise AssertionError(f"unexpected gh {arguments}")

        with (
            patch.object(repair.sync_pr, "gh", side_effect=gh),
            patch.object(repair.sync_pr, "enable_auto_merge") as enable,
        ):
            pushed, report = self.push()
        return pushed, report, calls, enable

    def test_a_github_change_turns_auto_merge_off_for_review(self):
        os.environ["SYNC_AUTO_MERGE"] = "true"
        commit = self.update({
            "fork.txt": "fixed\n",
            ".github/workflows/ci.yml": "changed\n",
        })
        pushed, report, calls, enable = self.push_with_auto_merge({
            "mergeMethod": "MERGE"
        })
        self.assertTrue(pushed)
        self.assertEqual(self.tip(), commit)
        self.assertIn(("pr", "merge", "7", "--disable-auto"), calls)
        enable.assert_not_called()
        body = report.call_args.args[1]
        self.assertIn("Auto-merge is off until you review it", body)
        self.assertIn("- `.github/workflows/ci.yml`", body)

    def test_a_github_change_without_auto_merge_only_reports(self):
        _ = self.update({".github/workflows/ci.yml": "changed\n"})
        pushed, report, calls, enable = self.push_with_auto_merge()
        self.assertTrue(pushed)
        self.assertNotIn("merge", [call[1] for call in calls])
        enable.assert_not_called()
        report.assert_called_once()

    def test_other_changes_turn_auto_merge_on(self):
        os.environ["SYNC_AUTO_MERGE"] = "true"
        for name in ("fork.txt", "script/upstream-sync/resolve.sh", "script/clippy"):
            with self.subTest(name=name):
                _ = git(
                    self.work,
                    "push",
                    "-q",
                    "--force",
                    "origin",
                    f"{self.head}:refs/heads/sync/upstream",
                )
                commit = self.update({name: "changed\n"})
                pushed, report, _, enable = self.push_with_auto_merge(heads=[commit])
                self.assertTrue(pushed)
                enable.assert_called_once_with("7", commit)
                report.assert_not_called()

    def test_auto_merge_waits_for_the_pr_to_show_the_update(self):
        os.environ["SYNC_AUTO_MERGE"] = "true"
        commit = self.update({"fork.txt": "fixed\n"})
        _, _, calls, enable = self.push_with_auto_merge(
            heads=[self.head, self.head, commit]
        )
        self.assertEqual(sum(call[:2] == ("pr", "view") for call in calls), 3)
        enable.assert_called_once_with("7", commit)

    def test_a_pr_that_never_shows_the_update_fails(self):
        os.environ["SYNC_AUTO_MERGE"] = "true"
        _ = self.update({"fork.txt": "fixed\n"})
        with self.assertRaisesRegex(ValueError, "never showed"):
            _ = self.push_with_auto_merge(heads=[self.head] * repair.HEAD_ATTEMPTS)

    def test_auto_merge_needs_the_sync_token(self):
        _ = self.update({"fork.txt": "fixed\n"})
        pushed, report, calls, enable = self.push_with_auto_merge()
        self.assertTrue(pushed)
        self.assertEqual(calls, [])
        enable.assert_not_called()
        report.assert_not_called()

    def test_an_update_for_another_head_is_refused(self):
        _ = self.update(
            {"fork.txt": "fixed\n"}, message="Update\n\nSync-Update: " + "0" * 40
        )
        with self.assertRaisesRegex(ValueError, "not for"):
            _ = self.push()
        self.assertEqual(self.tip(), self.head)

    def test_a_moved_branch_rejects_the_push(self):
        _ = self.update({"fork.txt": "fixed\n"})
        self.write("other.txt", "someone else\n")
        moved = self.commit("moved")
        _ = git(self.work, "push", "-q", "origin", "sync/upstream")
        pushed, report = self.push()
        self.assertFalse(pushed)
        body = report.call_args.args[1]
        self.assertIn("GitHub rejected the push", body)
        self.assertIn("stale info", body)
        self.assertEqual(self.tip(), moved)

    def test_a_failed_update_is_reported(self):
        _ = (self.directory / "result").write_text("failed\n")
        _ = (self.directory / "report.md").write_text("## Automatic update failed\n")
        pushed, report = self.push()
        self.assertFalse(pushed)
        self.assertIn("## Automatic update failed", report.call_args.args[1])
        self.assertEqual(self.tip(), self.head)

    def test_a_missing_result_is_reported(self):
        pushed, report = self.push()
        self.assertFalse(pushed)
        self.assertIn("The update produced no result.", report.call_args.args[1])


if __name__ == "__main__":
    _ = unittest.main()
