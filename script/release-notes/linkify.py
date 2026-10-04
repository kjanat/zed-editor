#!/usr/bin/env python3

"""Write the GitHub references in release notes as explicit Markdown links."""

import json
import re
import subprocess
import sys
from collections.abc import Callable, Iterable
from dataclasses import dataclass
from functools import cache
from pathlib import Path
from typing import Literal

PROTECTED = re.compile(
    r"^```.*?^```|`[^`\n]*`|\[[^\]\n]*\]\([^)\n]*\)|<a\b[^>]*>.*?</a>|<[^>\n]*>|https?://[^\s)]+",
    re.DOTALL | re.MULTILINE,
)
REFERENCE = re.compile(
    r"(?<![\w./@#-])"
    r"(?:(?:(?P<owner>[A-Za-z0-9-]+)/(?P<name>[A-Za-z0-9._-]+))?#(?P<number>\d+)"
    r"|(?P<commit_owner>[A-Za-z0-9-]+)/(?P<commit_name>[A-Za-z0-9._-]+)@(?P<commit>[0-9a-f]{7,40})"
    r"|(?P<sha>(?=[0-9a-f]*[a-f])(?=[0-9a-f]*[0-9])[0-9a-f]{7,40}))"
    r"(?![\w-])"
)
GRAPHQL_BATCH = 100

Kind = Literal["pull", "issues"]


@dataclass(frozen=True)
class Repository:
    owner: str
    name: str

    def __str__(self) -> str:
        return f"{self.owner}/{self.name}"


@dataclass(frozen=True)
class Number:
    repository: Repository
    number: int


@dataclass(frozen=True)
class Commit:
    repository: Repository
    sha: str


@dataclass(frozen=True)
class Resolved:
    numbers: dict[Number, Kind]
    commits: set[Commit]


Resolver = Callable[[set[Number], set[Commit]], Resolved]


@dataclass(frozen=True)
class Repositories:
    fork: Repository
    parent: Repository | None

    def commit_owners(self) -> tuple[Repository, ...]:
        return (self.fork,) if self.parent is None else (self.parent, self.fork)


class UnresolvedReferences(Exception):
    pass


def unprotected(text: str) -> Iterable[tuple[bool, str]]:
    position = 0
    for protected in PROTECTED.finditer(text):
        yield False, text[position : protected.start()]
        yield True, protected.group()
        position = protected.end()
    yield False, text[position:]


def reference(match: re.Match[str], default: Repository) -> Number | Commit:
    if match["number"] is not None:
        repository = (
            Repository(match["owner"], match["name"]) if match["owner"] else default
        )
        return Number(repository, int(match["number"]))
    if match["commit"] is not None:
        return Commit(
            Repository(match["commit_owner"], match["commit_name"]), match["commit"]
        )
    return Commit(default, match["sha"])


@dataclass(frozen=True)
class References:
    numbers: set[Number]
    commits: set[Commit]
    named_commits: set[Commit]


def references(text: str, repositories: Repositories) -> References:
    found = References(set(), set(), set())
    for protected, segment in unprotected(text):
        if protected:
            continue
        for match in REFERENCE.finditer(segment):
            match reference(match, repositories.fork):
                case Number() as number:
                    found.numbers.add(number)
                case Commit() as commit if match["commit"] is not None:
                    found.commits.add(commit)
                    found.named_commits.add(commit)
                case Commit(_, sha):
                    found.commits.update(
                        Commit(owner, sha) for owner in repositories.commit_owners()
                    )
    return found


def commit_link(commit: Commit) -> str:
    return (
        f'<a href="https://github.com/{commit.repository}/commit/{commit.sha}">'
        f"{commit.repository.owner}@<tt>{commit.sha}</tt></a>"
    )


def link(match: re.Match[str], repositories: Repositories, resolved: Resolved) -> str:
    match reference(match, repositories.fork):
        case Number(repository, number) as found:
            kind = resolved.numbers[found]
            return f"[{repository}#{number}](https://github.com/{repository}/{kind}/{number})"
        case Commit() as commit if match["commit"] is not None:
            return commit_link(commit)
        case Commit(_, sha):
            for owner in repositories.commit_owners():
                if Commit(owner, sha) in resolved.commits:
                    return commit_link(Commit(owner, sha))
    return match.group()


def linkify(text: str, repositories: Repositories, resolve: Resolver) -> str:
    found = references(text, repositories)
    resolved = resolve(found.numbers, found.commits)
    missing = sorted(
        f"{number.repository}#{number.number}"
        for number in found.numbers - resolved.numbers.keys()
    )
    missing += sorted(
        f"{commit.repository}@{commit.sha}"
        for commit in found.named_commits - resolved.commits
    )
    if missing:
        raise UnresolvedReferences(", ".join(missing))
    return "".join(
        segment
        if protected
        else REFERENCE.sub(lambda match: link(match, repositories, resolved), segment)
        for protected, segment in unprotected(text)
    )


def graphql(query: str) -> dict[str, object]:
    output = subprocess.run(
        ["gh", "api", "graphql", "--field", f"query={query}"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    return json.loads(output)["data"]["repository"]


def on_default_branch(commit: Commit) -> bool:
    repository = commit.repository
    result = subprocess.run(
        [
            "gh",
            "api",
            f"repos/{repository}/compare/{default_branch(repository)}...{commit.sha}",
            "--jq",
            ".status",
        ],
        check=False,
        capture_output=True,
        text=True,
    )
    return result.returncode == 0 and result.stdout.strip() in ("behind", "identical")


@cache
def default_branch(repository: Repository) -> str:
    return subprocess.run(
        ["gh", "api", f"repos/{repository}", "--jq", ".default_branch"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout.strip()


def resolve_with_github(numbers: set[Number], commits: set[Commit]) -> Resolved:
    resolved = Resolved({}, {commit for commit in commits if on_default_branch(commit)})
    for repository in sorted({number.repository for number in numbers}, key=str):
        fields = [
            (
                f"n{number.number}",
                f"issueOrPullRequest(number: {number.number}) {{ __typename }}",
                number,
            )
            for number in sorted(numbers, key=lambda number: number.number)
            if number.repository == repository
        ]
        for start in range(0, len(fields), GRAPHQL_BATCH):
            batch = fields[start : start + GRAPHQL_BATCH]
            selection = " ".join(f"{alias}: {field}" for alias, field, _ in batch)
            data = graphql(
                f'query {{ repository(owner: "{repository.owner}", name: "{repository.name}") {{ {selection} }} }}'
            )
            for alias, _, number in batch:
                match data.get(alias):
                    case {"__typename": "PullRequest"}:
                        resolved.numbers[number] = "pull"
                    case {"__typename": "Issue"}:
                        resolved.numbers[number] = "issues"
                    case _:
                        pass
    return resolved


def current_repositories() -> Repositories:
    view = json.loads(
        subprocess.run(
            ["gh", "repo", "view", "--json", "owner,name,parent"],
            check=True,
            capture_output=True,
            text=True,
        ).stdout
    )
    parent = view["parent"]
    return Repositories(
        Repository(view["owner"]["login"], view["name"]),
        Repository(parent["owner"]["login"], parent["name"]) if parent else None,
    )


def main(paths: list[str]) -> int:
    if not paths:
        print("usage: linkify.py NOTES.md...", file=sys.stderr)
        return 2
    repositories = current_repositories()
    status = 0
    for path in map(Path, paths):
        try:
            text = linkify(path.read_text(), repositories, resolve_with_github)
        except UnresolvedReferences as error:
            print(f"{path}: unresolved references: {error}", file=sys.stderr)
            status = 1
            continue
        path.write_text(text)
    return status


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
