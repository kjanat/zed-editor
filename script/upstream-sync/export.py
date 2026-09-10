"""Copy only regular, bounded files out of the untrusted container output."""

import os
from pathlib import Path
import shutil
import stat
import sys


def export(source: Path, destination: Path):
    destination.mkdir()
    for name, limit in (
        ("result", 32),
        ("sync.bundle", 256 * 1024 * 1024),
        ("issue-body.md", 60000),
    ):
        try:
            descriptor = os.open(
                source / name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK
            )
        except FileNotFoundError:
            if name == "result":
                raise
            continue
        with os.fdopen(descriptor, "rb") as stream:
            metadata = os.fstat(stream.fileno())
            if not stat.S_ISREG(metadata.st_mode) or metadata.st_size > limit:
                raise ValueError(f"Invalid container output: {name}")
            with (destination / name).open("xb") as output:
                shutil.copyfileobj(stream, output)


if __name__ == "__main__":
    export(Path(sys.argv[1]), Path(sys.argv[2]))
