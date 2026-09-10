"""Publish a candidate as Git objects, never as executable checkout contents."""

import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

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
        issue = gh(
            "issue",
            "list",
            "--state",
            "open",
            "--search",
            f"{title} in:title",
            "--json",
            "number,title",
            "--jq",
            f'.[] | select(.title == "{title}") | .number',
        ).splitlines()
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
    if result == "conflict":
        report("Upstream sync conflict", (directory / "issue-body.md").read_text())
        return
    if result not in ("clean", "resolved"):
        raise ValueError("Invalid preparation result")

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
    body = (
        "Automated upstream sync: merges zed-industries/zed main into master.\n\n"
        "Merge preparation runs in an isolated container without runner credentials. "
        "The publisher validates the candidate without checking it out and rejects changes to `.github`. "
        "Passing CI does not replace review of the imported code.\n\n"
        "Release Notes:\n\n- N/A\n"
    )
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
