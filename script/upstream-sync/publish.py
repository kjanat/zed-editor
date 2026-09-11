"""Publish a candidate as Git objects."""

import html
import json
import os
import re
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import NotRequired, TypedDict, cast

REPOSITORY = "kjanat/zed-editor"
BRANCH = "sync/upstream"
ASSIGNEE = "kjanat"


class Issue(TypedDict):
    number: int
    title: str


class AssignedIssue(Issue):
    assignees: list[dict[str, str]]


class Check(TypedDict, total=False):
    name: str
    context: str
    conclusion: str | None


class PullRequestChecks(TypedDict):
    mergeStateStatus: str
    statusCheckRollup: NotRequired[list[Check] | None]


class PullRequestMerge(TypedDict):
    headRefOid: str
    state: str
    autoMergeRequest: dict[str, str] | None


def run(*arguments: str, cwd: Path | None = None) -> str:
    return subprocess.check_output(
        arguments, cwd=cwd, text=True, stderr=subprocess.PIPE
    ).strip()


def git(*arguments: str) -> str:
    return run("git", "-c", "core.hooksPath=/dev/null", *arguments)


def validate(candidate: Path, base: str):
    _ = git("bundle", "verify", str(candidate))
    _ = git(
        "-c",
        "fetch.fsckObjects=true",
        "fetch",
        "--no-tags",
        str(candidate),
        f"refs/heads/{BRANCH}:refs/sync-candidate",
    )
    head = git("rev-parse", "refs/sync-candidate^{commit}")
    _ = git("merge-base", "--is-ancestor", base, head)
    # Tree equality includes additions, deletions, renames, file modes and symlinks.
    if git("diff", "--name-only", base, head, "--", ".github"):
        raise ValueError(
            "Candidate changes .github; review automation changes manually before syncing"
        )
    return head


def gh(*arguments: str) -> str:
    return run("gh", *arguments, "--repo", REPOSITORY)


def is_conflict_title(title: str):
    return (
        re.fullmatch(
            r"Upstream sync conflict(?: \([0-9]{4}-[0-9]{2}-[0-9]{2}\))?", title
        )
        is not None
    )


def report(title: str, body: str):
    with tempfile.NamedTemporaryFile(mode="w", suffix=".md") as message:
        _ = message.write(body)
        message.flush()
        issues = cast(
            list[AssignedIssue],
            json.loads(
                gh(
                    "issue",
                    "list",
                    "--state",
                    "open",
                    "--search",
                    f"{title} in:title",
                    "--json",
                    "number,title,assignees",
                    "--limit",
                    "1000",
                )
            ),
        )
        issue = [
            issue
            for issue in issues
            if issue["title"] == title
            or (title == "Upstream sync conflict" and is_conflict_title(issue["title"]))
        ]
        if issue:
            number = str(issue[0]["number"])
            edits = [] if issue[0]["assignees"] else ["--add-assignee", ASSIGNEE]
            if title == "Upstream sync conflict":
                edits.extend(["--add-label", "upstream-sync-conflict"])
            if edits:
                _ = gh("issue", "edit", number, *edits)
            _ = gh("issue", "comment", number, "--body-file", message.name)
        else:
            _ = gh(
                "issue",
                "create",
                "--title",
                title,
                "--body-file",
                message.name,
                "--assignee",
                ASSIGNEE,
                *(
                    ("--label", "upstream-sync-conflict")
                    if title == "Upstream sync conflict"
                    else ()
                ),
            )


def inspect_sync_pr():
    number = gh(
        "pr",
        "list",
        "--head",
        BRANCH,
        "--base",
        "master",
        "--state",
        "open",
        "--json",
        "number",
        "--jq",
        ".[0].number // empty",
    )
    if not number:
        return number
    details: PullRequestChecks = {"mergeStateStatus": "UNKNOWN"}
    for attempt in range(3):
        details = cast(
            PullRequestChecks,
            json.loads(
                gh(
                    "pr",
                    "view",
                    number,
                    "--json",
                    "mergeStateStatus,statusCheckRollup",
                )
            ),
        )
        if details["mergeStateStatus"] != "UNKNOWN" or attempt == 2:
            break
        time.sleep(10)
    failed = sorted({
        check.get("name", check.get("context", "Unnamed check"))
        for check in (details.get("statusCheckRollup") or [])
        if (check.get("conclusion") or "").upper()
        in {"FAILURE", "TIMED_OUT", "STARTUP_FAILURE", "ACTION_REQUIRED"}
    })
    state = details["mergeStateStatus"]
    if state in {"DIRTY", "BLOCKED"} or failed:
        report(
            "Upstream sync needs attention",
            f"Sync PR: #{number}\n\nMerge state: `{state}`\n\n"
            + f"Failing checks: {', '.join(failed) or 'none'}\n\n"
            + "The next candidate will still be attempted. If the same checks fail again, review the sync PR.",
        )
    return number


def resolution_details(directory: Path):
    sections = ["## Resolved automatically (verify, do not resolve)\n"]
    for name, heading, description in (
        ("formatting-only.txt", "Formatting-only", "upstream taken and reformatted"),
        (
            "formatted-three-way.txt",
            "Formatted three-way",
            "fork and upstream changes retained",
        ),
        ("lockfiles.txt", "Lockfile", "fork side kept and reconciled by cargo"),
    ):
        paths = (directory / name).read_text().splitlines()
        if paths:
            sections.append(f"### {heading}\n")
            sections.append(
                "\n".join(
                    f"- <code>{html.escape(path)}</code>: {description}"
                    for path in paths
                )
            )
    return "\n\n".join(sections) + "\n\n"


def sync_pr_body(resolutions: str = "", *, auto_merge: bool = False) -> str:
    conflicts = cast(
        list[Issue],
        json.loads(
            gh(
                "issue",
                "list",
                "--state",
                "open",
                "--label",
                "upstream-sync-conflict",
                "--limit",
                "1000",
                "--json",
                "number,title",
            )
        ),
    )
    references = "".join(
        f"Closes #{int(issue['number'])}.\n"
        for issue in conflicts
        if is_conflict_title(issue["title"])
    )
    return (
        "Automated upstream sync: merges zed-industries/zed main into master.\n\n"
        + "Merge preparation runs in an isolated container without runner credentials. "
        + "The publisher validates the candidate without checking it out and rejects changes to `.github`. "
        + (
            "Auto-merge is enabled with a merge commit once the required checks pass.\n\n"
            if auto_merge
            else "Auto-merge was not requested; required checks and merging need manual action.\n\n"
        )
        + resolutions
        + references
        + ("\n" if references else "")
        + "Release Notes:\n\n- N/A\n"
    )


def enable_auto_merge(pull_request: str, head: str):
    details = cast(
        PullRequestMerge,
        json.loads(
            gh(
                "pr",
                "view",
                pull_request,
                "--json",
                "headRefOid,state,autoMergeRequest",
            )
        ),
    )
    if details["headRefOid"] != head:
        raise ValueError("Sync PR head does not match the validated candidate")
    if details["state"] == "MERGED":
        print("Sync PR is already merged")
        return
    if details["state"] != "OPEN":
        raise ValueError("Sync PR is not open")
    if (request := details["autoMergeRequest"]) is not None:
        if request["mergeMethod"] != "MERGE":
            raise ValueError("Existing sync auto-merge must use a merge commit")
        print("Sync PR already has auto-merge enabled")
        return
    _ = gh(
        "pr",
        "merge",
        pull_request,
        "--auto",
        "--merge",
        "--match-head-commit",
        head,
    )


def publish(directory: Path):
    if os.environ.get("GITHUB_REPOSITORY") != REPOSITORY:
        raise ValueError("Unexpected repository")
    auto_merge = os.environ.get("SYNC_AUTO_MERGE") == "true"
    base = os.environ["GITHUB_SHA"]
    if not re.fullmatch(r"[0-9a-f]{40}", base):
        raise ValueError("Invalid trusted base")
    result = (directory / "result").read_text().strip()
    if result == "unchanged":
        print("No upstream changes")
        return
    number = inspect_sync_pr()
    if result == "conflict":
        report("Upstream sync conflict", (directory / "issue-body.md").read_text())
        return
    if result not in ("clean", "resolved"):
        raise ValueError("Invalid preparation result")

    resolutions = resolution_details(directory) if result == "resolved" else ""

    _ = git("fetch", "origin", "master", "--no-tags")
    if git("rev-parse", "FETCH_HEAD") != base:
        raise ValueError(
            "master moved during preparation; run sync again on the current master"
        )
    try:
        head = validate(directory / "sync.bundle", base)
    except ValueError as error:
        report("Upstream sync requires security review", str(error))
        raise

    previous = git("ls-remote", "origin", f"refs/heads/{BRANCH}").split()
    previous_head = previous[0] if previous else ""
    if previous_head:
        _ = git(
            "fetch", "origin", f"refs/heads/{BRANCH}:refs/sync-previous", "--no-tags"
        )
        if git("rev-parse", "refs/sync-previous") != previous_head:
            raise ValueError("Sync branch changed while checking it")
        # Upstream commits are expected to retain their original authors.
        _ = git(
            "fetch", "https://github.com/zed-industries/zed.git", "main", "--no-tags"
        )
        upstream = git("rev-parse", "FETCH_HEAD")
        authors = git(
            "log", "--format=%an", previous_head, "--not", base, upstream
        ).splitlines()
        if any(author != "github-actions[bot]" for author in authors):
            report(
                "Upstream sync needs attention",
                "The sync branch contains local commits; it was not overwritten.",
            )
            return

    _ = git(
        "-c",
        "credential.helper=",
        "-c",
        "credential.helper=!gh auth git-credential",
        "push",
        f"--force-with-lease=refs/heads/{BRANCH}:{previous_head}",
        "origin",
        f"{head}:refs/heads/{BRANCH}",
    )
    if git("ls-remote", "origin", f"refs/heads/{BRANCH}").split()[0] != head:
        raise ValueError("Published branch does not match the validated candidate")
    body = sync_pr_body(resolutions, auto_merge=auto_merge)
    with tempfile.NamedTemporaryFile(mode="w", suffix=".md") as message:
        _ = message.write(body)
        message.flush()
        if number:
            assignee_count = gh(
                "pr",
                "view",
                number,
                "--json",
                "assignees",
                "--jq",
                ".assignees | length",
            )
            _ = gh(
                "pr",
                "edit",
                number,
                "--body-file",
                message.name,
                *(("--add-assignee", ASSIGNEE) if assignee_count == "0" else ()),
            )
        else:
            _ = gh(
                "pr",
                "create",
                "--head",
                BRANCH,
                "--base",
                "master",
                "--title",
                "Merge upstream",
                "--body-file",
                message.name,
                "--label",
                "build",
                "--assignee",
                ASSIGNEE,
            )
    if auto_merge:
        enable_auto_merge(number or BRANCH, head)
    else:
        print("Auto-merge skipped: SYNC_TOKEN is not configured")


if __name__ == "__main__":
    publish(Path(sys.argv[1]).resolve())
