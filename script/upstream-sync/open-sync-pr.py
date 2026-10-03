"""Open or update the upstream sync PR, or the conflict issue, from a validated candidate."""

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


def is_repeatable(arguments: tuple[str, ...]) -> bool:
    if arguments[0] == "gh":
        return arguments[1:3] in (
            ("issue", "list"),
            ("pr", "list"),
            ("pr", "view"),
            ("pr", "edit"),
        )
    return arguments[0] == "git" and ("fetch" in arguments or "ls-remote" in arguments)


def run(*arguments: str, cwd: Path | None = None) -> str:
    for attempt in range(1, 3 if is_repeatable(arguments) else 1):
        try:
            return subprocess.check_output(
                arguments, cwd=cwd, text=True, stderr=subprocess.PIPE
            ).strip()
        except subprocess.CalledProcessError:
            time.sleep(10 * attempt)
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


def open_pr_from(branch: str) -> str:
    return gh(
        "pr",
        "list",
        "--head",
        branch,
        "--base",
        "master",
        "--state",
        "open",
        "--json",
        "number",
        "--jq",
        ".[0].number // empty",
    )


def any_open_sync_pr() -> str:
    return gh(
        "pr",
        "list",
        "--base",
        "master",
        "--state",
        "open",
        "--json",
        "number,headRefName",
        "--jq",
        '[.[] | select(.headRefName | startswith("sync/upstream"))][0].number // empty',
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
        (
            "structured.txt",
            "Structured merge",
            "fork and upstream changes retained",
        ),
        ("lockfiles.txt", "Lockfile", "fork side kept, then cargo reconciled it"),
        ("fork-deleted.txt", "Deleted in the fork", "upstream changes dropped"),
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


def claude_resolution(directory: Path):
    summary = directory / "resolution.md"
    if not summary.exists():
        return ""
    return (
        "## Claude's resolution\n\n"
        + "The merge passed the full validation before this PR was opened.\n\n"
        + html.escape(summary.read_text(), quote=False).strip()
        + "\n\n"
    )


def dropped_automation(directory: Path):
    dropped = directory / "dropped-github.txt"
    paths = dropped.read_text().splitlines() if dropped.exists() else []
    if not paths:
        return ""
    return (
        "## Upstream automation dropped (port by hand if wanted)\n\n"
        + "\n".join(f"- <code>{html.escape(path)}</code>" for path in paths)
        + "\n\n"
    )


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
        + "Merge preparation runs in an isolated container. "
        + "`open-sync-pr.py` validates this merge without checking it out and rejects any change to `.github`. "
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
    for attempt in range(1, 4):
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
        try:
            _ = gh(
                "pr",
                "merge",
                pull_request,
                "--auto",
                "--merge",
                "--match-head-commit",
                head,
            )
            return
        except subprocess.CalledProcessError:
            if attempt == 3:
                raise
            time.sleep(10 * attempt)


def open_sync_pr(directory: Path):
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
    _ = inspect_sync_pr()
    verify = os.environ.get("SYNC_VERIFY")
    if result == "partial" and verify == "skipped":
        if pull_request := any_open_sync_pr():
            print(
                f"Sync PR #{pull_request} is open; Claude resolves again after it merges"
            )
            return
        report(
            "Upstream sync conflict",
            (directory / "conflict-report.md").read_text()
            + f"\nClaude already tried `master` at {base} and failed. "
            + "Close this issue to let the next run try again.\n",
        )
        return
    if result == "partial" and verify not in (None, "success"):
        run_url = "{}/{}/actions/runs/{}".format(
            os.environ.get("GITHUB_SERVER_URL", "https://github.com"),
            REPOSITORY,
            os.environ.get("GITHUB_RUN_ID", ""),
        )
        report(
            "Upstream sync conflict",
            (directory / "conflict-report.md").read_text()
            + "\n## Automatic resolution failed\n\n"
            + f"The `verify` job ended with `{verify}`. See {run_url}.\n\n"
            + f"<!-- claude-attempt fork={base} -->\n",
        )
        return
    if result == "conflict":
        report("Upstream sync conflict", (directory / "issue-body.md").read_text())
        return
    if result not in ("clean", "resolved"):
        raise ValueError("Invalid preparation result")

    resolutions = resolution_details(directory) if result == "resolved" else ""
    resolutions += claude_resolution(directory)
    resolutions += dropped_automation(directory)

    _ = git("fetch", "origin", "master", "--no-tags")
    master = git("rev-parse", "FETCH_HEAD")
    if master != base:
        try:
            _ = git("merge-base", "--is-ancestor", base, master)
        except subprocess.CalledProcessError:
            raise ValueError(
                f"master was rewritten during the run and no longer contains {base}"
            ) from None
    try:
        head = validate(directory / "sync.bundle", base)
    except ValueError as error:
        report("Upstream sync requires security review", str(error))
        raise

    branch = BRANCH
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
            branch = f"{BRANCH}-{head[:12]}"
            resolutions = (
                f"`{BRANCH}` has commits that `github-actions[bot]` did not make, "
                + f"so this PR comes from `{branch}`.\n\n"
                + resolutions
            )
            previous = git("ls-remote", "origin", f"refs/heads/{branch}").split()
            previous_head = previous[0] if previous else ""

    _ = git(
        "-c",
        "credential.helper=",
        "-c",
        "credential.helper=!gh auth git-credential",
        "push",
        f"--force-with-lease=refs/heads/{branch}:{previous_head}",
        "origin",
        f"{head}:refs/heads/{branch}",
    )
    if git("ls-remote", "origin", f"refs/heads/{branch}").split()[0] != head:
        raise ValueError("Pushed branch does not match the validated candidate")
    number = open_pr_from(branch)
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
                branch,
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
        enable_auto_merge(number or branch, head)
    else:
        print("Auto-merge skipped: SYNC_TOKEN is not configured")


if __name__ == "__main__":
    open_sync_pr(Path(sys.argv[1]).resolve())
