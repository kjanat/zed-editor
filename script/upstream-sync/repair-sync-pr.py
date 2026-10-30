"""Hand an open sync PR to Claude once per head, and push Claude's verified update to it."""

import importlib.util
import json
import os
import re
import subprocess
import sys
import time
from pathlib import Path
from types import ModuleType
from typing import TypedDict, cast

TITLE = "Upstream sync needs attention"
UPSTREAM = "https://github.com/zed-industries/zed.git"
BRANCH = re.compile(r"sync/upstream(-[0-9a-f]{12})?")
SHA = re.compile(r"[0-9a-f]{40}")
FAILED = {"failure", "timed_out", "startup_failure"}
LOG_LIMIT = 300_000
REPORT_LIMIT = 60_000
HEAD_ATTEMPTS = 6


class PullRequestHead(TypedDict):
    headRefName: str
    headRefOid: str


class AutoMerge(TypedDict):
    autoMergeRequest: dict[str, str] | None


class Run(TypedDict):
    databaseId: int
    status: str
    conclusion: str


def load_sync_pr() -> ModuleType:
    spec = importlib.util.spec_from_file_location(
        "open_sync_pr", Path(__file__).with_name("open-sync-pr.py")
    )
    if spec is None or spec.loader is None:
        raise ImportError("Cannot load open-sync-pr.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


sync_pr = load_sync_pr()


def marker(head: str, upstream: str) -> str:
    return f"<!-- claude-update head={head} upstream={upstream} -->"


def checked(branch: str, head: str) -> tuple[str, str]:
    if not BRANCH.fullmatch(branch):
        raise ValueError(f"Not a sync branch: {branch}")
    if not SHA.fullmatch(head):
        raise ValueError("Invalid head commit")
    return branch, head


def output(**values: str):
    with Path(os.environ["GITHUB_OUTPUT"]).open("a") as file:
        for name, value in values.items():
            _ = file.write(f"{name}={value}\n")


def branch_tip(branch: str) -> str:
    remote = sync_pr.git("ls-remote", "origin", f"refs/heads/{branch}").split()
    return remote[0] if remote else ""


def attempted(head: str, upstream: str) -> bool:
    bodies = sync_pr.gh(
        "issue",
        "list",
        "--state",
        "open",
        "--search",
        f"{TITLE} in:title",
        "--json",
        "title,body,comments",
        "--jq",
        f'.[] | select(.title == "{TITLE}") | .body, .comments[].body',
    )
    return marker(head, upstream) in bodies


def gate(directory: Path) -> bool:
    if os.environ.get("GITHUB_REPOSITORY") != sync_pr.REPOSITORY:
        raise ValueError("Unexpected repository")
    number = sync_pr.any_open_sync_pr()
    if not number:
        output(open="false", run="false")
        return True
    details = cast(
        PullRequestHead,
        json.loads(
            sync_pr.gh("pr", "view", number, "--json", "headRefName,headRefOid")
        ),
    )
    branch, head = checked(details["headRefName"], details["headRefOid"])
    output(open="true", head=head, branch=branch)
    runs = cast(
        list[Run],
        json.loads(
            sync_pr.gh(
                "run",
                "list",
                "--workflow",
                "CI",
                "--commit",
                head,
                "--json",
                "databaseId,status,conclusion",
                "--limit",
                "1",
            )
        ),
    )
    if runs and runs[0]["status"] != "completed":
        sync_pr.announce(f"CI is still running on sync PR #{number}.")
        output(run="false")
        return True
    failed = bool(runs) and runs[0]["conclusion"] in FAILED
    _ = sync_pr.git("fetch", "--no-tags", "origin", head)
    _ = sync_pr.git("fetch", "--no-tags", UPSTREAM, "main")
    upstream = sync_pr.git("rev-parse", "FETCH_HEAD")
    behind = sync_pr.git("rev-list", "--count", f"{head}..{upstream}") != "0"
    if not failed and not behind:
        sync_pr.announce(f"Sync PR #{number} passes CI and has every upstream commit.")
        output(run="false")
        return True
    if attempted(head, upstream):
        sync_pr.announce(
            f"Claude already tried {head} against upstream {upstream}. "
            + "New upstream commits, a push to the PR or closing the issue allow another attempt."
        )
        output(run="false")
        return True
    work = [
        *(["fix the failed CI run"] if failed else []),
        *([f"merge upstream {upstream}"] if behind else []),
    ]
    issue = sync_pr.report(
        TITLE,
        f"Sync PR: #{number}\n\nClaude updates {head} once: {' and '.join(work)}.\n\n"
        + f"{marker(head, upstream)}\n",
    )
    sync_pr.announce(f"Claude updates sync PR #{number} once. Tracked in {issue}")
    directory.mkdir(parents=True, exist_ok=True)
    if failed:
        log = sync_pr.gh("run", "view", str(runs[0]["databaseId"]), "--log-failed")
        _ = (directory / "failed.log").write_bytes(log.encode()[-LOG_LIMIT:])
    output(run="true", upstream=upstream if behind else "")
    return True


def hold_for_review(number: str, update: str, workflows: list[str]):
    details = cast(
        AutoMerge,
        json.loads(sync_pr.gh("pr", "view", number, "--json", "autoMergeRequest")),
    )
    if details["autoMergeRequest"] is not None:
        _ = sync_pr.gh("pr", "merge", number, "--disable-auto")
    files = "".join(f"- `{path}`\n" for path in workflows)
    issue = sync_pr.report(
        TITLE,
        f"Sync PR: #{number}\n\nClaude's update {update} changes `.github`. "
        + f"Auto-merge is off until you review it:\n\n{files}",
    )
    sync_pr.announce(
        f"Auto-merge is off for sync PR #{number}. Review requested in {issue}"
    )


def merge_when_green(number: str, update: str):
    for attempt in range(HEAD_ATTEMPTS):
        details = cast(
            PullRequestHead,
            json.loads(
                sync_pr.gh("pr", "view", number, "--json", "headRefName,headRefOid")
            ),
        )
        if details["headRefOid"] == update:
            sync_pr.enable_auto_merge(number, update)
            sync_pr.announce(f"Auto-merge is on for sync PR #{number}.")
            return
        if attempt < HEAD_ATTEMPTS - 1:
            time.sleep(10)
    raise ValueError(f"Sync PR #{number} never showed {update} as its head")


def push(directory: Path) -> bool:
    if os.environ.get("GITHUB_REPOSITORY") != sync_pr.REPOSITORY:
        raise ValueError("Unexpected repository")
    branch, head = checked(os.environ["HEAD_BRANCH"], os.environ["HEAD_SHA"])
    upstream = os.environ.get("UPSTREAM", "")
    if upstream and not SHA.fullmatch(upstream):
        raise ValueError("Invalid upstream commit")
    result = directory / "result"
    if not result.exists() or result.read_text().strip() != "repaired":
        report = directory / "report.md"
        body = (
            report.read_text()[:REPORT_LIMIT]
            if report.exists()
            else "The update produced no result.\n"
        )
        issue = sync_pr.report(TITLE, f"Claude's update of {head} failed.\n\n{body}")
        sync_pr.announce(f"Claude's update failed. Reported in {issue}")
        return False
    bundle = directory / "repair.bundle"
    _ = sync_pr.git("fetch", "--no-tags", "origin", head)
    _ = sync_pr.git("bundle", "verify", str(bundle))
    _ = sync_pr.git(
        "-c",
        "fetch.fsckObjects=true",
        "fetch",
        "--no-tags",
        str(bundle),
        "+refs/sync/repaired:refs/sync-update",
    )
    update = sync_pr.git("rev-parse", "refs/sync-update^{commit}")
    parents = " ".join(filter(None, (head, upstream)))
    if sync_pr.git("rev-list", "--parents", "-n", "1", update) != f"{update} {parents}":
        raise ValueError(f"The update does not have exactly {parents} as its parents")
    if f"Sync-Update: {head}" not in sync_pr.git("log", "-1", "--format=%B", update):
        raise ValueError(f"The update is not for {head}")
    number = sync_pr.open_pr_from(branch)
    if not number:
        raise ValueError(f"`{branch}` has no open sync PR")
    workflows = sync_pr.git(
        "diff", "--name-only", head, update, "--", ".github"
    ).splitlines()
    if workflows:
        hold_for_review(number, update, workflows)
    try:
        _ = sync_pr.git(
            "-c",
            "credential.helper=",
            "-c",
            "credential.helper=!gh auth git-credential",
            "push",
            f"--force-with-lease=refs/heads/{branch}:{head}",
            "origin",
            f"{update}:refs/heads/{branch}",
        )
    except subprocess.CalledProcessError as error:
        issue = sync_pr.report(
            TITLE,
            f"GitHub rejected the push of Claude's update {update} to `{branch}`:\n\n"
            + f"```\n{(error.stderr or '').strip()[:REPORT_LIMIT]}\n```\n",
        )
        sync_pr.announce(
            f"The push of Claude's update was rejected. Reported in {issue}"
        )
        return False
    if branch_tip(branch) != update:
        raise ValueError("Pushed branch does not match the verified update")
    sync_pr.announce(f"Pushed Claude's update {update} to `{branch}`.")
    if not workflows and os.environ.get("SYNC_AUTO_MERGE") == "true":
        merge_when_green(number, update)
    return True


if __name__ == "__main__":
    commands = {"gate": gate, "push": push}
    if len(sys.argv) != 3 or sys.argv[1] not in commands:
        sys.exit("usage: repair-sync-pr.py gate|push <directory>")
    sys.exit(0 if commands[sys.argv[1]](Path(sys.argv[2]).resolve()) else 1)
