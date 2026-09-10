"""Publish a candidate as Git objects, never as executable checkout contents."""

import json
import html
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time

REPOSITORY = "kjanat/zed-editor"
BRANCH = "sync/upstream"


def run(*arguments, cwd=None):
    return subprocess.check_output(
        arguments, cwd=cwd, text=True, stderr=subprocess.PIPE
    ).strip()


def git(*arguments):
    return run("git", "-c", "core.hooksPath=/dev/null", *arguments)


def validate(candidate: Path, base: str):
    git("bundle", "verify", str(candidate))
    git(
        "-c",
        "fetch.fsckObjects=true",
        "fetch",
        "--no-tags",
        str(candidate),
        f"refs/heads/{BRANCH}:refs/sync-candidate",
    )
    head = git("rev-parse", "refs/sync-candidate^{commit}")
    git("merge-base", "--is-ancestor", base, head)
    # Tree equality includes additions, deletions, renames, file modes and symlinks.
    if git("diff", "--name-only", base, head, "--", ".github"):
        raise ValueError(
            "Candidate changes .github; review automation changes manually before syncing"
        )
    return head


def gh(*arguments):
    return run("gh", *arguments, "--repo", REPOSITORY)


def report(title: str, body: str):
    with tempfile.NamedTemporaryFile(mode="w", suffix=".md") as message:
        message.write(body)
        message.flush()
        issues = json.loads(
            gh(
                "issue",
                "list",
                "--state",
                "open",
                "--search",
                f"{title} in:title",
                "--json",
                "number,title",
                "--limit",
                "1000",
            )
        )
        issue = [
            str(issue["number"])
            for issue in issues
            if issue["title"] == title
            or (
                title == "Upstream sync conflict"
                and re.fullmatch(
                    r"Upstream sync conflict \(\d{4}-\d{2}-\d{2}\)", issue["title"]
                )
            )
        ]
        if issue:
            gh("issue", "comment", issue[0], "--body-file", message.name)
        else:
            gh(
                "issue",
                "create",
                "--title",
                title,
                "--body-file",
                message.name,
                "--label",
                "upstream-sync-conflict",
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
    for attempt in range(3):
        details = json.loads(
            gh(
                "pr",
                "view",
                number,
                "--json",
                "mergeStateStatus,statusCheckRollup",
            )
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
            f"Failing checks: {', '.join(failed) or 'none'}\n\n"
            "The next candidate will still be attempted. If the same checks fail again, review the sync PR.",
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


def sync_pr_body(resolutions=""):
    conflicts = gh(
        "issue",
        "list",
        "--state",
        "open",
        "--label",
        "upstream-sync-conflict",
        "--limit",
        "1000",
        "--json",
        "number",
        "--jq",
        ".[].number",
    ).splitlines()
    references = "".join(f"Closes #{int(number)}.\n" for number in conflicts)
    return (
        "Automated upstream sync: merges zed-industries/zed main into master.\n\n"
        "Merge preparation runs in an isolated container without runner credentials. "
        "The publisher validates the candidate without checking it out and rejects changes to `.github`. "
        "Passing CI does not replace review of the imported code.\n\n"
        + resolutions
        + references
        + ("\n" if references else "")
        + "Release Notes:\n\n- N/A\n"
    )


def publish(directory: Path):
    if os.environ.get("GITHUB_REPOSITORY") != REPOSITORY:
        raise ValueError("Unexpected repository")
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

    git("fetch", "origin", "master", "--no-tags")
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
        git("fetch", "origin", f"refs/heads/{BRANCH}:refs/sync-previous", "--no-tags")
        if git("rev-parse", "refs/sync-previous") != previous_head:
            raise ValueError("Sync branch changed while checking it")
        # Upstream commits are expected to retain their original authors.
        git("fetch", "https://github.com/zed-industries/zed.git", "main", "--no-tags")
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

    git(
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
    body = sync_pr_body(resolutions)
    with tempfile.NamedTemporaryFile(mode="w", suffix=".md") as message:
        message.write(body)
        message.flush()
        if number:
            gh("pr", "edit", number, "--body-file", message.name)
        else:
            gh(
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
            )


if __name__ == "__main__":
    publish(Path(sys.argv[1]).resolve())
