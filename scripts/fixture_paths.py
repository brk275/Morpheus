"""Keep disposable Matrix credentials outside source checkouts."""

from __future__ import annotations

import os
import tempfile
from pathlib import Path


REPOSITORY = Path(__file__).resolve().parent.parent


def external_path(path: Path) -> Path:
    """Reject checkout paths, including existing symlinks into the checkout."""
    resolved = path.expanduser().resolve()
    if resolved.is_relative_to(REPOSITORY):
        raise ValueError("Disposable fixture data must be stored outside the repository")
    return resolved


def fixture_home() -> Path:
    return external_path(Path(os.environ.get(
        "MORPHEUS_MATRIX_TEST_HOME",
        str(Path(tempfile.gettempdir()) / "morpheus-matrix-test"),
    )))


def private_directory(path: Path) -> Path:
    path = external_path(path)
    path.mkdir(mode=0o700, parents=True, exist_ok=True)
    os.chmod(path, 0o700)
    return path


def write_private(path: Path, content: str, *, create: bool = False) -> None:
    """Atomically write owner-only data without following a destination link."""
    external_path(path)
    if path.is_symlink():
        raise ValueError("Refusing to write fixture data through a symbolic link")
    parent = private_directory(path.parent)
    fd, name = tempfile.mkstemp(prefix=".fixture-", dir=parent)
    temporary = Path(name)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as file:
            file.write(content)
        os.chmod(temporary, 0o600)
        external_path(path)
        if create:
            os.link(temporary, path)
        else:
            temporary.replace(path)
    finally:
        temporary.unlink(missing_ok=True)
