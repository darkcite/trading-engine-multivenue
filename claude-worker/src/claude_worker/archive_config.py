# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""Configuration and enablement for the object-storage archive (phase S2).

Two jobs, both about failing safe:

**Enablement.** The subsystem is opt-in and, when it is off, every code path
above it must be a no-op — no HTTP client constructed, no bucket contacted, and
every existing behaviour byte-identical to a deployment that has never heard of
S3 (S-LAW 2). ``load`` therefore never raises on missing or malformed
configuration: it returns a disabled config carrying a one-word ``reason``. A
loader that threw would turn "the operator has not set this up" into a crash in
the nightly report.

**Redaction.** The secret lives in exactly one place — a
``dataclasses.field(repr=False)`` inside ``objstore.Credentials`` — and is read
only from the process environment or the credential file (S-LAW 6). No shell
script sources that file; nothing here ever puts a value into argv, a log line,
a manifest, a report, a URL, or an exception message. ``reason`` is drawn from a
fixed vocabulary precisely so that it cannot become a channel for a value.

Precedence: the process environment wins, the file fills the gaps. That order
lets a test inject a mapping and never touch the operator's files, and lets a
launchd job override one key without rewriting the file.

Convention: full ``import x`` only. No ``from x import y``.
"""

import collections.abc
import dataclasses
import os
import pathlib
import re
import typing

import claude_worker.objstore

ENV_FILE_KEY: typing.Final[str] = "MULTIVENUE_S3_ENV_FILE"
DEFAULT_ENV_FILE: typing.Final[str] = "~/multivenue/s3.env"

#: Explicit, never derived from the machine name: macOS renames LocalHostName
#: on conflict, which would silently split the archive across two host ids.
HOST_ID_RE: typing.Final[re.Pattern[str]] = re.compile(r"^[a-z0-9][a-z0-9-]{0,31}$")

#: The six keys without which nothing can work.
REQUIRED_KEYS: typing.Final[tuple[str, ...]] = (
    "MULTIVENUE_S3_BUCKET",
    "MULTIVENUE_S3_ENDPOINT",
    "MULTIVENUE_S3_REGION",
    "MULTIVENUE_S3_HOST_ID",
    "MULTIVENUE_S3_ACCESS_KEY_ID",
    "MULTIVENUE_S3_SECRET_ACCESS_KEY",
)

#: `reason` is a closed vocabulary. It is printed in logs and reports, so it
#: must never be able to carry a configured value.
REASON_ENABLED: typing.Final[str] = "enabled"
REASON_FLAG: typing.Final[str] = "flag"
REASON_HOST_ID: typing.Final[str] = "bad host_id"
REASON_MISSING: typing.Final[dict[str, str]] = {
    "MULTIVENUE_S3_BUCKET": "missing bucket",
    "MULTIVENUE_S3_ENDPOINT": "missing endpoint",
    "MULTIVENUE_S3_REGION": "missing region",
    "MULTIVENUE_S3_HOST_ID": "missing host_id",
    "MULTIVENUE_S3_ACCESS_KEY_ID": "missing credentials",
    "MULTIVENUE_S3_SECRET_ACCESS_KEY": "missing credentials",
}

#: Shortest string that can be a quoted value: the two quotes themselves.
_QUOTED_MIN_LEN: typing.Final[int] = 2

DEFAULTS: typing.Final[dict[str, str]] = {
    "MULTIVENUE_S3_ADDRESSING": "virtual",
    "MULTIVENUE_S3_PREFIX": "v1",
    "MULTIVENUE_S3_CACHE_DIR": "~/multivenue/s3-cache",
    "MULTIVENUE_S3_CACHE_MAX_GIB": "20",
    "MULTIVENUE_S3_CACHE_MIN_FREE_GIB": "25",
    "MULTIVENUE_S3_ZSTD_LEVEL": "3",
    # 32 MiB. Two measurements bound this from both sides:
    #   * multipart costs ~3x a single PUT on this endpoint (2.29 vs 7.36 MiB/s
    #     measured), and that penalty is per PART — so bigger parts are faster.
    #   * a part must still finish inside TIMEOUT_S on a BAD network window.
    #     The link is highly variable here (0.1 MiB/s observed at one point,
    #     5-7 MiB/s at another), so 32 MiB against a 900 s timeout survives
    #     roughly 0.04 MiB/s, while the 64 MiB the plan first proposed blew a
    #     120 s timeout outright.
    "MULTIVENUE_S3_PART_SIZE_MIB": "32",
    "MULTIVENUE_S3_CONCURRENCY": "1",
    "MULTIVENUE_S3_TIMEOUT_S": "900",
    "MULTIVENUE_S3_SKIP_GLOBS": "",
}


@dataclasses.dataclass(frozen=True, slots=True)
class ArchiveConfig:
    enabled: bool
    reason: str
    endpoint: claude_worker.objstore.Endpoint | None
    creds: claude_worker.objstore.Credentials | None
    host_id: str
    prefix: str
    cache_dir: pathlib.Path
    cache_max_gib: int
    cache_min_free_gib: int
    zstd_level: int
    part_size_mib: int
    concurrency: int
    timeout_s: float
    skip_globs: tuple[str, ...]

    @property
    def part_size(self) -> int:
        return self.part_size_mib * 1024 * 1024

    def runs_prefix(self) -> str:
        return f"{self.prefix}/runs/{self.host_id}/"

    def index_prefix(self) -> str:
        return f"{self.prefix}/index/{self.host_id}/"

    def tarballs_prefix(self) -> str:
        return f"{self.prefix}/tarballs/{self.host_id}/"

    def derived_prefix(self, kind: str) -> str:
        return f"{self.prefix}/derived/{self.host_id}/{kind}/"

    def run_key(self, run_id: str, name: str) -> str:
        return f"{self.runs_prefix()}{run_id}/{name}.zst"

    def manifest_key(self, run_id: str) -> str:
        return f"{self.runs_prefix()}{run_id}/_manifest.json"

    def index_key(self, run_id: str) -> str:
        return f"{self.index_prefix()}{run_id}.json"

    def staging_dir(self, run_id: str) -> pathlib.Path:
        return self.cache_dir / "staging" / run_id

    def manifest_cache_dir(self) -> pathlib.Path:
        return self.cache_dir / "manifests"

    def tell(self) -> str:
        """The house-style one-liner. Carries names and counts, never values."""
        if not self.enabled:
            return f"archive: disabled ({self.reason})"
        bucket = self.endpoint.bucket if self.endpoint else ""
        return f"archive: enabled bucket={bucket} host={self.host_id}"


def read_env_file(path: pathlib.Path) -> dict[str, str]:
    """``KEY=VALUE`` with ``#`` comments and optional quotes. No expansion.

    Deliberately not a shell parser: this file is never sourced by a shell
    (S-LAW 6), so giving it shell semantics would only invite someone to try.
    """
    out: dict[str, str] = {}
    try:
        text = path.read_text(encoding="utf-8")
    except (OSError, UnicodeDecodeError):
        # An unreadable credential file means "disabled", never a crash — and
        # never an error message that could echo a line of the file.
        return out
    for raw in text.splitlines():
        line = raw.strip()
        if not line or line.startswith("#") or "=" not in line:
            continue
        name, _, value = line.partition("=")
        name = name.strip()
        value = value.strip()
        if len(value) >= _QUOTED_MIN_LEN and value[0] == value[-1] and value[0] in "\"'":
            value = value[1:-1]
        if name:
            out[name] = value
    return out


def _int(source: collections.abc.Mapping[str, str], key: str) -> int:
    raw = source.get(key, "") or DEFAULTS[key]
    try:
        return int(raw)
    except ValueError:
        return int(DEFAULTS[key])


def _float(source: collections.abc.Mapping[str, str], key: str) -> float:
    raw = source.get(key, "") or DEFAULTS[key]
    try:
        return float(raw)
    except ValueError:
        return float(DEFAULTS[key])


def _disabled(reason: str, merged: collections.abc.Mapping[str, str]) -> ArchiveConfig:
    """A disabled config still carries the harmless knobs.

    `gc` and `status` remain meaningful with the subsystem off — they operate on
    the local cache, which exists whether or not a bucket does.
    """
    return ArchiveConfig(
        enabled=False,
        reason=reason,
        endpoint=None,
        creds=None,
        host_id=merged.get("MULTIVENUE_S3_HOST_ID", ""),
        prefix=merged.get("MULTIVENUE_S3_PREFIX", "") or DEFAULTS["MULTIVENUE_S3_PREFIX"],
        cache_dir=pathlib.Path(
            merged.get("MULTIVENUE_S3_CACHE_DIR", "") or DEFAULTS["MULTIVENUE_S3_CACHE_DIR"]
        ).expanduser(),
        cache_max_gib=_int(merged, "MULTIVENUE_S3_CACHE_MAX_GIB"),
        cache_min_free_gib=_int(merged, "MULTIVENUE_S3_CACHE_MIN_FREE_GIB"),
        zstd_level=_int(merged, "MULTIVENUE_S3_ZSTD_LEVEL"),
        part_size_mib=_int(merged, "MULTIVENUE_S3_PART_SIZE_MIB"),
        concurrency=_int(merged, "MULTIVENUE_S3_CONCURRENCY"),
        timeout_s=_float(merged, "MULTIVENUE_S3_TIMEOUT_S"),
        skip_globs=_globs(merged.get("MULTIVENUE_S3_SKIP_GLOBS", "")),
    )


def _globs(raw: str) -> tuple[str, ...]:
    return tuple(part.strip() for part in raw.split(",") if part.strip())


def load(
    env: collections.abc.Mapping[str, str] | None = None,
    env_file: pathlib.Path | None = None,
) -> ArchiveConfig:
    """Merge the environment over the credential file. Never raises.

    Mirrors ``config.load_base_from_env(env)`` so tests inject a mapping and
    never read the operator's real files.
    """
    source: collections.abc.Mapping[str, str] = os.environ if env is None else env
    if env_file is None:
        env_file = pathlib.Path(
            source.get(ENV_FILE_KEY, "") or DEFAULT_ENV_FILE
        ).expanduser()

    merged: dict[str, str] = read_env_file(env_file)
    for key, value in source.items():
        if key.startswith("MULTIVENUE_S3_"):
            merged[key] = value

    if merged.get("MULTIVENUE_S3_ENABLED", "") != "1":
        return _disabled(REASON_FLAG, merged)
    for key in REQUIRED_KEYS:
        if not merged.get(key, ""):
            return _disabled(REASON_MISSING[key], merged)

    host_id = merged["MULTIVENUE_S3_HOST_ID"]
    if not HOST_ID_RE.match(host_id):
        return _disabled(REASON_HOST_ID, merged)

    addressing = merged.get("MULTIVENUE_S3_ADDRESSING", "") or DEFAULTS["MULTIVENUE_S3_ADDRESSING"]
    if addressing not in ("virtual", "path"):
        addressing = DEFAULTS["MULTIVENUE_S3_ADDRESSING"]

    base = _disabled(REASON_ENABLED, merged)
    return dataclasses.replace(
        base,
        enabled=True,
        endpoint=claude_worker.objstore.Endpoint(
            host=merged["MULTIVENUE_S3_ENDPOINT"],
            region=merged["MULTIVENUE_S3_REGION"],
            bucket=merged["MULTIVENUE_S3_BUCKET"],
            addressing=addressing,
        ),
        creds=claude_worker.objstore.Credentials(
            access_key_id=merged["MULTIVENUE_S3_ACCESS_KEY_ID"],
            secret_access_key=merged["MULTIVENUE_S3_SECRET_ACCESS_KEY"],
        ),
        host_id=host_id,
    )


def is_enabled(cfg: ArchiveConfig) -> bool:
    return cfg.enabled and cfg.endpoint is not None and cfg.creds is not None
