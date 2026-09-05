# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""Gate G-S2 — enablement (the S-LAW 2 proof) and secret redaction.

Two properties are worth more than the rest of this file combined:

1. **Disabled means disabled.** Drop any one of the six required keys and the
   subsystem must report itself off, with a reason drawn from a closed
   vocabulary — never raise, never half-configure. Everything above this layer
   keys off that flag to stay byte-identical to a deployment with no archive.
2. **The secret cannot escape.** A sentinel value is loaded and then hunted for
   in every repr, str, log record and exception text the module can produce.

Every test injects its own mapping or a tmp_path file: none of them can read
the operator's real credential file.

Convention: full ``import x`` only. No ``from x import y``.
"""

import logging
import pathlib
import typing

import pytest

import claude_worker.archive_config
import claude_worker.objstore

SENTINEL: typing.Final[str] = "SENTINEL-SECRET-a1b2c3-do-not-log"

FULL_ENV: typing.Final[dict[str, str]] = {
    "MULTIVENUE_S3_ENABLED": "1",
    "MULTIVENUE_S3_BUCKET": "market-data-qu",
    "MULTIVENUE_S3_ENDPOINT": "fsn1.your-objectstorage.com",
    "MULTIVENUE_S3_REGION": "fsn1",
    "MULTIVENUE_S3_HOST_ID": "mbp-m4",
    "MULTIVENUE_S3_ACCESS_KEY_ID": "AKIAEXAMPLEARCHIVE00",
    "MULTIVENUE_S3_SECRET_ACCESS_KEY": SENTINEL,
}


def nowhere(tmp_path: pathlib.Path) -> pathlib.Path:
    """A credential-file path that deliberately does not exist."""
    return tmp_path / "absent" / "s3.env"


def test_enabled_with_every_required_key(tmp_path: pathlib.Path) -> None:
    cfg = claude_worker.archive_config.load(env=FULL_ENV, env_file=nowhere(tmp_path))
    assert cfg.enabled
    assert claude_worker.archive_config.is_enabled(cfg)
    assert cfg.reason == claude_worker.archive_config.REASON_ENABLED
    assert cfg.endpoint is not None
    assert cfg.endpoint.bucket == "market-data-qu"
    assert cfg.endpoint.addressing == "virtual"


@pytest.mark.parametrize("missing", claude_worker.archive_config.REQUIRED_KEYS)
def test_disabled_when_any_required_key_absent(missing: str, tmp_path: pathlib.Path) -> None:
    env = dict(FULL_ENV)
    del env[missing]
    cfg = claude_worker.archive_config.load(env=env, env_file=nowhere(tmp_path))
    assert not cfg.enabled
    assert not claude_worker.archive_config.is_enabled(cfg)
    assert cfg.reason == claude_worker.archive_config.REASON_MISSING[missing]
    assert cfg.endpoint is None and cfg.creds is None


def test_disabled_when_flag_is_not_one(tmp_path: pathlib.Path) -> None:
    for value in ("0", "", "true", "yes", "1 "):
        env = dict(FULL_ENV, MULTIVENUE_S3_ENABLED=value)
        cfg = claude_worker.archive_config.load(env=env, env_file=nowhere(tmp_path))
        assert not cfg.enabled, value
        assert cfg.reason == claude_worker.archive_config.REASON_FLAG


def test_missing_env_file_is_disabled_not_an_error(tmp_path: pathlib.Path) -> None:
    cfg = claude_worker.archive_config.load(env={}, env_file=nowhere(tmp_path))
    assert not cfg.enabled
    assert cfg.reason == claude_worker.archive_config.REASON_FLAG


def test_unreadable_env_file_is_disabled_not_an_error(tmp_path: pathlib.Path) -> None:
    """A directory where a file should be: the loader must shrug, not crash."""
    path = tmp_path / "s3.env"
    path.mkdir()
    cfg = claude_worker.archive_config.load(env={}, env_file=path)
    assert not cfg.enabled


def test_env_wins_over_file(tmp_path: pathlib.Path) -> None:
    path = tmp_path / "s3.env"
    path.write_text(
        "\n".join(f"{k}={v}" for k, v in FULL_ENV.items()).replace(
            "market-data-qu", "from-the-file"
        ),
        encoding="utf-8",
    )
    cfg = claude_worker.archive_config.load(
        env={"MULTIVENUE_S3_BUCKET": "from-the-environment"}, env_file=path
    )
    assert cfg.enabled
    assert cfg.endpoint is not None
    assert cfg.endpoint.bucket == "from-the-environment"


def test_file_fills_the_gaps_the_environment_leaves(tmp_path: pathlib.Path) -> None:
    path = tmp_path / "s3.env"
    path.write_text("\n".join(f"{k}={v}" for k, v in FULL_ENV.items()), encoding="utf-8")
    cfg = claude_worker.archive_config.load(env={}, env_file=path)
    assert cfg.enabled
    assert cfg.host_id == "mbp-m4"


def test_env_file_key_names_the_file(tmp_path: pathlib.Path) -> None:
    path = tmp_path / "elsewhere.env"
    path.write_text("\n".join(f"{k}={v}" for k, v in FULL_ENV.items()), encoding="utf-8")
    cfg = claude_worker.archive_config.load(
        env={claude_worker.archive_config.ENV_FILE_KEY: str(path)}
    )
    assert cfg.enabled


def test_read_env_file_handles_comments_blanks_and_quotes(tmp_path: pathlib.Path) -> None:
    path = tmp_path / "s3.env"
    path.write_text(
        "# a comment\n\nA=1\n  B = two  \nC=\"quoted\"\nD='single'\nnot a pair\nE=has=equals\n",
        encoding="utf-8",
    )
    assert claude_worker.archive_config.read_env_file(path) == {
        "A": "1",
        "B": "two",
        "C": "quoted",
        "D": "single",
        "E": "has=equals",
    }


@pytest.mark.parametrize(
    "host_id,ok",
    [
        ("mbp-m4", True),
        ("a", True),
        ("0", True),
        ("a" * 32, True),
        ("a" * 33, False),
        ("-leading", False),
        ("Antons-MacBook-Pro", False),
        ("has_underscore", False),
        ("has.dot", False),
        ("", False),
    ],
)
def test_host_id_regex(host_id: str, ok: bool, tmp_path: pathlib.Path) -> None:
    env = dict(FULL_ENV, MULTIVENUE_S3_HOST_ID=host_id)
    cfg = claude_worker.archive_config.load(env=env, env_file=nowhere(tmp_path))
    assert cfg.enabled is ok
    if not ok:
        assert cfg.reason in (
            claude_worker.archive_config.REASON_HOST_ID,
            claude_worker.archive_config.REASON_MISSING["MULTIVENUE_S3_HOST_ID"],
        )


def test_defaults_are_the_measured_values(tmp_path: pathlib.Path) -> None:
    cfg = claude_worker.archive_config.load(env=FULL_ENV, env_file=nowhere(tmp_path))
    # Bounded from both sides by measurement: multipart costs ~3x a single PUT
    # per PART (2.29 vs 7.36 MiB/s), so bigger is faster; but a part must still
    # finish inside the timeout on a bad network window, and this link has been
    # seen at 0.1 MiB/s. 32 MiB against 900 s survives ~0.04 MiB/s.
    assert cfg.part_size_mib == 32
    assert cfg.part_size == 32 * 1024 * 1024
    assert cfg.part_size >= claude_worker.objstore.MIN_PART_SIZE
    assert cfg.timeout_s == 900.0
    assert cfg.zstd_level == 3
    assert cfg.concurrency == 1
    assert cfg.cache_max_gib == 20
    assert cfg.cache_min_free_gib == 25
    assert cfg.skip_globs == ()


def test_malformed_numeric_values_fall_back_to_defaults(tmp_path: pathlib.Path) -> None:
    env = dict(FULL_ENV, MULTIVENUE_S3_PART_SIZE_MIB="not-a-number", MULTIVENUE_S3_TIMEOUT_S="")
    cfg = claude_worker.archive_config.load(env=env, env_file=nowhere(tmp_path))
    assert cfg.enabled
    assert cfg.part_size_mib == 32
    assert cfg.timeout_s == 900.0


def test_unknown_addressing_falls_back_to_virtual(tmp_path: pathlib.Path) -> None:
    env = dict(FULL_ENV, MULTIVENUE_S3_ADDRESSING="nonsense")
    cfg = claude_worker.archive_config.load(env=env, env_file=nowhere(tmp_path))
    assert cfg.endpoint is not None and cfg.endpoint.addressing == "virtual"


def test_skip_globs_parses_a_csv(tmp_path: pathlib.Path) -> None:
    env = dict(FULL_ENV, MULTIVENUE_S3_SKIP_GLOBS=" *-raw.tap , *.tmp ,, ")
    cfg = claude_worker.archive_config.load(env=env, env_file=nowhere(tmp_path))
    assert cfg.skip_globs == ("*-raw.tap", "*.tmp")


def test_key_helpers_build_the_documented_layout(tmp_path: pathlib.Path) -> None:
    cfg = claude_worker.archive_config.load(env=FULL_ENV, env_file=nowhere(tmp_path))
    run = "run-1788417289611943000"
    assert cfg.run_key(run, "pm-ticks.pmlr") == (
        f"v1/runs/mbp-m4/{run}/pm-ticks.pmlr.zst"
    )
    assert cfg.manifest_key(run) == f"v1/runs/mbp-m4/{run}/_manifest.json"
    assert cfg.index_key(run) == f"v1/index/mbp-m4/{run}.json"
    assert cfg.runs_prefix() == "v1/runs/mbp-m4/"
    assert cfg.index_prefix() == "v1/index/mbp-m4/"


def test_every_generated_key_is_representable_without_escaping(tmp_path: pathlib.Path) -> None:
    cfg = claude_worker.archive_config.load(env=FULL_ENV, env_file=nowhere(tmp_path))
    run = "run-1788417289611943000"
    for key in (
        cfg.run_key(run, "okx-opt-summary.pmlr"),
        cfg.manifest_key(run),
        cfg.index_key(run),
        cfg.tarballs_prefix() + run + ".tar.gz",
        cfg.derived_prefix("candles") + "candles-20260905.db.zst",
    ):
        assert claude_worker.objstore.KEY_RE.match(key), key


def test_tell_reports_names_never_values(tmp_path: pathlib.Path) -> None:
    cfg = claude_worker.archive_config.load(env=FULL_ENV, env_file=nowhere(tmp_path))
    assert cfg.tell() == "archive: enabled bucket=market-data-qu host=mbp-m4"
    assert SENTINEL not in cfg.tell()
    off = claude_worker.archive_config.load(env={}, env_file=nowhere(tmp_path))
    assert off.tell() == "archive: disabled (flag)"


def test_reason_is_a_closed_vocabulary(tmp_path: pathlib.Path) -> None:
    """A reason must never be able to carry a configured value into a log."""
    allowed = {
        claude_worker.archive_config.REASON_ENABLED,
        claude_worker.archive_config.REASON_FLAG,
        claude_worker.archive_config.REASON_HOST_ID,
        *claude_worker.archive_config.REASON_MISSING.values(),
    }
    for missing in claude_worker.archive_config.REQUIRED_KEYS:
        env = dict(FULL_ENV)
        del env[missing]
        cfg = claude_worker.archive_config.load(env=env, env_file=nowhere(tmp_path))
        assert cfg.reason in allowed
    weird = dict(FULL_ENV, MULTIVENUE_S3_HOST_ID="NOT/A/VALID/HOST")
    cfg = claude_worker.archive_config.load(env=weird, env_file=nowhere(tmp_path))
    assert cfg.reason in allowed
    assert "NOT/A/VALID/HOST" not in cfg.reason


def test_secret_never_in_repr_str_log_or_exception(
    tmp_path: pathlib.Path, caplog: pytest.LogCaptureFixture
) -> None:
    """The whole point of S-LAW 6, asserted rather than asserted-to."""
    path = tmp_path / "s3.env"
    path.write_text("\n".join(f"{k}={v}" for k, v in FULL_ENV.items()), encoding="utf-8")

    with caplog.at_level(logging.DEBUG):
        cfg = claude_worker.archive_config.load(env={}, env_file=path)
        assert cfg.enabled and cfg.creds is not None
        assert cfg.creds.secret_access_key == SENTINEL  # it IS loaded...

    # ...and it appears nowhere it could be read back out.
    assert SENTINEL not in repr(cfg)
    assert SENTINEL not in str(cfg)
    assert SENTINEL not in repr(cfg.creds)
    assert SENTINEL not in str(cfg.creds)
    assert SENTINEL not in cfg.tell()
    for record in caplog.records:
        assert SENTINEL not in record.getMessage()

    error = claude_worker.objstore.ObjStoreError(
        "HEAD v1/index/mbp-m4/x.json: HTTP 403 AccessDenied"
    )
    assert SENTINEL not in str(error)
    assert SENTINEL not in repr(error)


def test_disabled_config_still_answers_cache_questions(tmp_path: pathlib.Path) -> None:
    """`gc` and `status` stay meaningful with the bucket off: the cache is local."""
    cfg = claude_worker.archive_config.load(
        env={"MULTIVENUE_S3_CACHE_DIR": str(tmp_path / "cache")}, env_file=nowhere(tmp_path)
    )
    assert not cfg.enabled
    assert cfg.cache_dir == tmp_path / "cache"
    assert cfg.cache_max_gib == 20
