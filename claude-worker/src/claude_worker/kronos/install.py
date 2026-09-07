# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""K1.2 — the pinned-artifact installer for the Kronos lanes
(plan §3a.2 / §14.2, ruling D13).

``python -m claude_worker.kronos install`` fetches the two kinds of
runtime artifact named by ``claude-worker/kronos.lock``:

* the **upstream snapshot** — the four files of ``shiyu-coder/Kronos`` at
  the pinned commit (``model/__init__.py``, ``model/kronos.py``,
  ``model/module.py``, ``LICENSE``) into
  ``~/multivenue/vendor/kronos/<upstream id>/``;
* the **weights** — one directory per checkpoint under
  ``~/multivenue/artifacts/kronos/<name>/``.

Laws this module exists to enforce:

* **Nothing is vendored into git.** Both trees live outside the repo; the
  lock is the only tracked artefact and it is data, not code.
* **A hash mismatch is a refusal, not a warning** (exit 3). The bytes are
  written to a temp file, hashed while streaming, and only then renamed
  into place — a torn or wrong download can never be observed as
  installed.
* **No network at run time.** Only this module fetches; the adapter
  modules read the installed trees and verify against the same lock.
* **Idempotent.** A second run hashes what is on disk and prints
  ``up to date`` without a single request.

Exit codes: 0 ok · 3 hash/size mismatch (nothing written) · 4 transport
failure · 5 unusable lock.

Convention: full ``import x`` only. No ``from x import y``.
"""

import argparse
import collections.abc
import datetime
import hashlib
import json
import os
import pathlib
import sys
import time
import tomllib
import typing

import httpx

LOCK_ENV: str = "CLAUDE_WORKER_KRONOS_LOCK"
VENDOR_ENV: str = "CLAUDE_WORKER_KRONOS_VENDOR"
ARTIFACTS_ENV: str = "CLAUDE_WORKER_KRONOS_ARTIFACTS"

VENDOR_DEFAULT: str = "~/multivenue/vendor/kronos"
ARTIFACTS_DEFAULT: str = "~/multivenue/artifacts/kronos"
MANIFEST_NAME: str = "installed.json"

RC_OK: int = 0
RC_MISMATCH: int = 3
RC_TRANSPORT: int = 4
RC_LOCK: int = 5

DOWNLOAD_TIMEOUT_S: float = 120.0
CHUNK_BYTES: int = 1 << 20


class LockError(Exception):
    """The lock file is missing, malformed, or names something unknown."""


def lock_path(env: collections.abc.Mapping[str, str] | None = None) -> pathlib.Path:
    """``claude-worker/kronos.lock`` — the repo copy by default, since the
    lock is tracked data that ships with the code that reads it."""
    environ = os.environ if env is None else env
    override = environ.get(LOCK_ENV, "")
    if override:
        return pathlib.Path(override).expanduser()
    return pathlib.Path(__file__).resolve().parents[3] / "kronos.lock"


def load_lock(path: pathlib.Path | None = None) -> dict[str, typing.Any]:
    """Parse and shape-check the lock. Raises :class:`LockError`."""
    target = lock_path() if path is None else path
    try:
        raw = target.read_bytes()
    except OSError as exc:
        raise LockError(f"kronos.lock unreadable at {target}: {exc}") from exc
    try:
        lock = tomllib.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, tomllib.TOMLDecodeError) as exc:
        raise LockError(f"kronos.lock is not valid TOML: {exc}") from exc
    upstream = lock.get("upstream")
    if not isinstance(upstream, dict):
        raise LockError("kronos.lock: [upstream] missing")
    for key in ("repo", "commit", "license"):
        if not isinstance(upstream.get(key), str):
            raise LockError(f"kronos.lock: [upstream].{key} missing")
    if not _files_of(upstream):
        raise LockError("kronos.lock: [[upstream.file]] missing")
    weights = lock.get("weights")
    if not isinstance(weights, dict) or not weights:
        raise LockError("kronos.lock: [weights.*] missing")
    for name, entry in typing.cast(dict[str, typing.Any], weights).items():
        if not isinstance(entry, dict):
            raise LockError(f"kronos.lock: [weights.{name!r}] is not a table")
        if not _files_of(typing.cast(dict[str, typing.Any], entry)):
            raise LockError(f"kronos.lock: [weights.{name!r}] has no files")
    return lock


def _files_of(table: dict[str, typing.Any]) -> list[dict[str, typing.Any]]:
    """The validated ``[[<table>.file]]`` rows — EMPTY when any row is
    malformed. A partially trusted file list is worse than none: the
    caller would install the rows it understood and silently skip the
    rest, which is exactly the state the hash pinning exists to prevent."""
    raw = table.get("file")
    if not isinstance(raw, list):
        return []
    out: list[dict[str, typing.Any]] = []
    for item in typing.cast(list[object], raw):
        if not isinstance(item, dict):
            return []
        entry = typing.cast(dict[str, typing.Any], item)
        shaped = (
            isinstance(entry.get("path"), str)
            and isinstance(entry.get("url"), str)
            and isinstance(entry.get("sha256"), str)
            and isinstance(entry.get("bytes"), int)
        )
        if not shaped:
            return []
        out.append(entry)
    return out


def upstream_id(lock: dict[str, typing.Any]) -> str:
    """Content id of the [upstream] block — the vendor directory name.

    Hashed over a CANONICAL rendering (commit + the files sorted by path,
    each with its sha256 and byte count) rather than the TOML bytes, so
    reformatting a comment in the lock never orphans an installed tree,
    while changing what is pinned always does."""
    upstream = typing.cast(dict[str, typing.Any], lock["upstream"])
    parts: list[str] = [str(upstream["commit"])]
    for entry in sorted(_files_of(upstream), key=lambda f: str(f["path"])):
        parts.append(f"{entry['path']} {entry['sha256']} {entry['bytes']}")
    return hashlib.sha256("\n".join(parts).encode("utf-8")).hexdigest()


def vendor_dir(lock: dict[str, typing.Any]) -> pathlib.Path:
    root = pathlib.Path(os.environ.get(VENDOR_ENV, "") or VENDOR_DEFAULT).expanduser()
    return root / upstream_id(lock)


def artifacts_root() -> pathlib.Path:
    return pathlib.Path(os.environ.get(ARTIFACTS_ENV, "") or ARTIFACTS_DEFAULT).expanduser()


def weights_dir(name: str) -> pathlib.Path:
    return artifacts_root() / name


def file_sha256(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        while True:
            chunk = handle.read(CHUNK_BYTES)
            if not chunk:
                break
            digest.update(chunk)
    return digest.hexdigest()


def check_installed(directory: pathlib.Path, files: list[dict[str, typing.Any]]) -> list[str]:
    """Paths that are absent, the wrong size, or the wrong hash."""
    bad: list[str] = []
    for entry in files:
        target = directory / str(entry["path"])
        try:
            size = target.stat().st_size
        except OSError:
            bad.append(str(entry["path"]))
            continue
        if size != int(entry["bytes"]) or file_sha256(target) != str(entry["sha256"]):
            bad.append(str(entry["path"]))
    return bad


def _download(
    client: httpx.Client,
    entry: dict[str, typing.Any],
    directory: pathlib.Path,
    log: collections.abc.Callable[[str], None],
) -> int:
    """Stream one file to a temp path, hash it, rename on a match. The
    temp file is removed on every failure path — a refusal leaves the
    install exactly as it was."""
    rel = str(entry["path"])
    want_sha = str(entry["sha256"])
    want_bytes = int(entry["bytes"])
    target = directory / rel
    target.parent.mkdir(parents=True, exist_ok=True)
    tmp = target.with_name(target.name + ".part")
    digest = hashlib.sha256()
    written = 0
    try:
        with client.stream("GET", str(entry["url"]), timeout=DOWNLOAD_TIMEOUT_S) as response:
            if response.status_code != httpx.codes.OK:
                log(f"install: {rel}: HTTP {response.status_code}")
                return RC_TRANSPORT
            with tmp.open("wb") as handle:
                for chunk in response.iter_bytes(CHUNK_BYTES):
                    handle.write(chunk)
                    digest.update(chunk)
                    written += len(chunk)
    except httpx.HTTPError as exc:
        tmp.unlink(missing_ok=True)
        log(f"install: {rel}: transport failure: {exc}")
        return RC_TRANSPORT
    except OSError as exc:
        tmp.unlink(missing_ok=True)
        log(f"install: {rel}: write failure: {exc}")
        return RC_TRANSPORT
    got_sha = digest.hexdigest()
    if written != want_bytes or got_sha != want_sha:
        tmp.unlink(missing_ok=True)
        log(
            f"install: REFUSED {rel}: expected {want_bytes} B sha256 {want_sha},"
            f" got {written} B sha256 {got_sha}"
        )
        return RC_MISMATCH
    tmp.replace(target)
    log(f"install: {rel}: {written} B ok")
    return RC_OK


class Group(typing.NamedTuple):
    """One installable directory: the upstream snapshot, or one weights
    entry."""

    name: str
    directory: pathlib.Path
    files: list[dict[str, typing.Any]]
    extra: dict[str, str]


def write_manifest(group: Group) -> None:
    payload: dict[str, typing.Any] = {
        "name": group.name,
        "installed_utc": datetime.datetime.now(datetime.UTC).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "files": {str(entry["path"]): str(entry["sha256"]) for entry in group.files},
    }
    payload.update(group.extra)
    (group.directory / MANIFEST_NAME).write_text(
        json.dumps(payload, indent=1, sort_keys=True), encoding="utf-8"
    )


def install_group(
    client: httpx.Client, group: Group, log: collections.abc.Callable[[str], None]
) -> int:
    bad = check_installed(group.directory, group.files)
    if not bad:
        log(f"install: {group.name}: up to date ({len(group.files)} files, {group.directory})")
        write_manifest(group)
        return RC_OK
    log(
        f"install: {group.name}: fetching {len(bad)} of {len(group.files)} files"
        f" into {group.directory}"
    )
    group.directory.mkdir(parents=True, exist_ok=True)
    for entry in group.files:
        if str(entry["path"]) not in bad:
            continue
        started = time.monotonic()
        code = _download(client, entry, group.directory, log)
        if code != RC_OK:
            return code
        elapsed = time.monotonic() - started
        if elapsed > 1.0:
            rate = int(entry["bytes"]) / elapsed / (1 << 20)
            log(f"install: {entry['path']}: {elapsed:.0f} s ({rate:.1f} MiB/s)")
    still_bad = check_installed(group.directory, group.files)
    if still_bad:
        log(f"install: REFUSED {group.name}: still wrong after fetch: {still_bad}")
        return RC_MISMATCH
    write_manifest(group)
    log(f"install: {group.name}: installed")
    return RC_OK


def weight_names(lock: dict[str, typing.Any], only: list[str] | None) -> list[str]:
    weights = typing.cast(dict[str, typing.Any], lock["weights"])
    names = sorted(weights)
    if not only:
        return names
    picked: list[str] = []
    for name in only:
        if name not in weights:
            raise LockError(f"kronos.lock has no weights entry {name!r} (have: {', '.join(names)})")
        picked.append(name)
        entry = typing.cast(dict[str, typing.Any], weights[name])
        tokenizer = entry.get("tokenizer")
        if isinstance(tokenizer, str) and tokenizer not in picked:
            picked.append(tokenizer)
    return picked


def run_install(
    lock: dict[str, typing.Any],
    client: httpx.Client,
    only: str,
    models: list[str] | None,
    log: collections.abc.Callable[[str], None],
) -> int:
    upstream = typing.cast(dict[str, typing.Any], lock["upstream"])
    if only in ("all", "upstream"):
        code = install_group(
            client,
            Group(
                f"upstream {upstream['repo']}@{str(upstream['commit'])[:7]}",
                vendor_dir(lock),
                _files_of(upstream),
                {"license": str(upstream["license"]), "commit": str(upstream["commit"])},
            ),
            log,
        )
        if code != RC_OK:
            return code
    if only in ("all", "weights"):
        weights = typing.cast(dict[str, typing.Any], lock["weights"])
        for name in weight_names(lock, models):
            entry = typing.cast(dict[str, typing.Any], weights[name])
            code = install_group(
                client,
                Group(
                    name,
                    weights_dir(name),
                    _files_of(entry),
                    {
                        "revision": str(entry.get("revision", "")),
                        "kind": str(entry.get("kind", "")),
                    },
                ),
                log,
            )
            if code != RC_OK:
                return code
    return RC_OK


def run_verify(lock: dict[str, typing.Any], log: collections.abc.Callable[[str], None]) -> int:
    upstream = typing.cast(dict[str, typing.Any], lock["upstream"])
    worst = RC_OK
    bad = check_installed(vendor_dir(lock), _files_of(upstream))
    log(f"verify: upstream {vendor_dir(lock)}: {'OK' if not bad else 'BAD ' + ', '.join(bad)}")
    if bad:
        worst = RC_MISMATCH
    weights = typing.cast(dict[str, typing.Any], lock["weights"])
    for name in sorted(weights):
        entry = typing.cast(dict[str, typing.Any], weights[name])
        directory = weights_dir(name)
        if not directory.exists():
            log(f"verify: {name}: not installed")
            continue
        bad = check_installed(directory, _files_of(entry))
        log(f"verify: {name}: {'OK' if not bad else 'BAD ' + ', '.join(bad)}")
        if bad:
            worst = RC_MISMATCH
    return worst


def make_client() -> httpx.Client:
    """Seam for tests (monkeypatched to a MockTransport-backed client)."""
    return httpx.Client(follow_redirects=True)


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="python -m claude_worker.kronos")
    sub = parser.add_subparsers(dest="verb", required=True)
    install = sub.add_parser("install", help="fetch + verify the pinned artifacts")
    install.add_argument("--only", choices=("all", "upstream", "weights"), default="all")
    install.add_argument(
        "--model",
        action="append",
        default=None,
        help="weights entry to install (repeatable; its tokenizer comes along). Default: all.",
    )
    install.add_argument("--lock", default=None)
    verify = sub.add_parser("verify", help="hash what is installed against the lock")
    verify.add_argument("--lock", default=None)
    where = sub.add_parser("where", help="print the resolved artifact paths")
    where.add_argument("--lock", default=None)
    args = parser.parse_args(argv)

    def log(line: str) -> None:
        sys.stderr.write(line + "\n")
        sys.stderr.flush()

    try:
        lock = load_lock(pathlib.Path(args.lock).expanduser() if args.lock else None)
    except LockError as exc:
        log(f"kronos: {exc}")
        return RC_LOCK

    if args.verb == "where":
        print(f"lock\t{lock_path() if not args.lock else args.lock}")
        print(f"upstream_id\t{upstream_id(lock)}")
        print(f"vendor\t{vendor_dir(lock)}")
        print(f"artifacts\t{artifacts_root()}")
        return RC_OK
    if args.verb == "verify":
        return run_verify(lock, log)
    try:
        with make_client() as client:
            return run_install(lock, client, args.only, args.model, log)
    except LockError as exc:
        log(f"kronos: {exc}")
        return RC_LOCK


if __name__ == "__main__":
    sys.exit(main())
