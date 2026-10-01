"""Select the workspace packages a change can affect."""

import argparse
import json
import re
import subprocess
from collections.abc import Iterable
from dataclasses import dataclass
from pathlib import PurePosixPath
from typing import Literal, NotRequired, TypedDict, cast

FULL_RUN = re.compile(
    r"Cargo\.(toml|lock)|rust-toolchain\.toml|\.cargo/.*|\.config/nextest\.toml"
    r"|script/clippy(\.ps1)?|script/fork-ci/.*|\.github/workflows/fork_ci\.yaml"
)
ASSET_PACKAGES = ("assets", "settings")

Scope = Literal["none", "all", "packages"]


class Dependency(TypedDict):
    name: str
    path: NotRequired[str]


class Manifest(TypedDict):
    id: str
    name: str
    manifest_path: str
    dependencies: list[Dependency]


class Metadata(TypedDict):
    workspace_root: str
    workspace_members: list[str]
    packages: list[Manifest]


@dataclass(frozen=True)
class Package:
    name: str
    directory: PurePosixPath
    dependencies: frozenset[PurePosixPath]


@dataclass(frozen=True)
class Plan:
    scope: Scope
    packages: tuple[str, ...]
    reason: str

    def outputs(self) -> str:
        package_args = " ".join(f"-p {name}" for name in self.packages)
        return (
            f"scope={self.scope}\n"
            f"packages={' '.join(self.packages)}\n"
            f"package_args={package_args}\n"
        )


def parse_packages(metadata: Metadata) -> list[Package]:
    root = PurePosixPath(metadata["workspace_root"])
    members = set(metadata["workspace_members"])
    packages: list[Package] = []
    for manifest in metadata["packages"]:
        if manifest["id"] not in members:
            continue
        directory = PurePosixPath(manifest["manifest_path"]).parent.relative_to(root)
        dependency_paths = (
            PurePosixPath(path)
            for dependency in manifest["dependencies"]
            if (path := dependency.get("path")) is not None
        )
        dependencies = frozenset(
            path.relative_to(root)
            for path in dependency_paths
            if path.is_relative_to(root)
        )
        packages.append(Package(manifest["name"], directory, dependencies))
    return packages


def owner(path: PurePosixPath, packages: Iterable[Package]) -> Package | None:
    owners = [
        package
        for package in packages
        if package.directory != PurePosixPath(".")
        and path.is_relative_to(package.directory)
    ]
    return max(owners, key=lambda package: len(package.directory.parts), default=None)


def dependents(changed: set[str], packages: list[Package]) -> set[str]:
    by_directory = {package.directory: package.name for package in packages}
    reverse: dict[str, set[str]] = {package.name: set() for package in packages}
    for package in packages:
        for dependency in package.dependencies:
            if dependency_name := by_directory.get(dependency):
                reverse[dependency_name].add(package.name)
    affected = set(changed)
    pending = list(changed)
    while pending:
        for dependent in reverse.get(pending.pop(), ()):
            if dependent not in affected:
                affected.add(dependent)
                pending.append(dependent)
    return affected


def plan(changed_files: list[str] | None, packages: list[Package]) -> Plan:
    if changed_files is None:
        return Plan("all", (), "no comparison base")
    if full := next((path for path in changed_files if FULL_RUN.fullmatch(path)), None):
        return Plan("all", (), f"{full} changed")
    names = {package.name for package in packages}
    changed = {
        package.name
        for path in changed_files
        if (package := owner(PurePosixPath(path), packages)) is not None
    }
    if any(path.startswith("assets/") for path in changed_files):
        changed.update(name for name in ASSET_PACKAGES if name in names)
    if not changed:
        return Plan("none", (), "no workspace package changed")
    affected = dependents(changed, packages)
    if affected == names:
        return Plan("all", (), "every package depends on the change")
    return Plan(
        "packages",
        tuple(sorted(affected)),
        f"changed: {' '.join(sorted(changed))}",
    )


def changed_files(base: str, head: str) -> list[str] | None:
    if not base or set(base) == {"0"}:
        return None
    output = subprocess.run(
        ["git", "diff", "--name-only", "--no-renames", "-z", base, head],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    return [path for path in output.split("\0") if path]


def load_metadata() -> Metadata:
    output = subprocess.run(
        ["cargo", "metadata", "--format-version", "1", "--no-deps"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    return cast(Metadata, json.loads(output))


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", default="")
    parser.add_argument("--head", default="HEAD")
    arguments = parser.parse_args()
    result = plan(
        changed_files(arguments.base, arguments.head), parse_packages(load_metadata())
    )
    print(f"reason={result.reason}")
    print(result.outputs(), end="")


if __name__ == "__main__":
    main()
