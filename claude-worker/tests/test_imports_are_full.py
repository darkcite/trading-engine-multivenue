"""Enforces the codebase rule: full ``import x`` only — never ``from x import y``.

This test walks every .py file under ``src/claude_worker/`` and ``tests/``,
tokenizes it, and fails if it sees a ``from X import Y`` statement.

The only exceptions allowed are ``from __future__ import annotations``
(which is a compiler directive, not a real import). There are none today,
but the check allows it so it doesn't silently break later.

Convention: full ``import x`` only. No ``from x import y``.
"""

import ast
import pathlib


_ROOT: pathlib.Path = pathlib.Path(__file__).resolve().parent.parent
_SEARCH_DIRS: tuple[pathlib.Path, ...] = (
    _ROOT / "src" / "claude_worker",
    _ROOT / "tests",
)


def _all_py_files() -> list[pathlib.Path]:
    out: list[pathlib.Path] = []
    for i in range(len(_SEARCH_DIRS)):
        d = _SEARCH_DIRS[i]
        if not d.exists():
            continue
        for p in d.rglob("*.py"):
            out.append(p)
    return out


def test_no_from_imports_anywhere() -> None:
    offenders: list[str] = []
    files = _all_py_files()
    for i in range(len(files)):
        path = files[i]
        source = path.read_text("utf-8")
        tree = ast.parse(source, filename=str(path))
        for node in ast.walk(tree):
            if isinstance(node, ast.ImportFrom):
                # __future__ imports are compiler directives, not real imports.
                if node.module == "__future__":
                    continue
                offenders.append(f"{path}:{node.lineno}: from {node.module} import ...")

    assert not offenders, (
        "`from ... import ...` is forbidden in this codebase. Use full "
        "`import x` form only. Offenders:\n  " + "\n  ".join(offenders)
    )


def test_search_dirs_actually_exist() -> None:
    """Guard against the import-style test silently passing because no files
    exist to scan (e.g. after a directory rename)."""
    found = _all_py_files()
    assert len(found) >= 4, f"expected to find Python files, got {found}"
