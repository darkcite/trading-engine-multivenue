# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""bin15_seed.py -- the BIN15 boot seed cut (O4b).

The member cannot price without 1 440 minutes of returns and cannot fit
a line without 60 settled pairs, and the engine restarts three times a
UTC day, so without a seed it spends most of its life blind. These tests
pin the four properties that make the seed trustworthy: it is
arithmetically the law the engine runs, it cannot see the future, it is
written in the grammar ``core_config::vrp`` already parses, and **a pair
belongs to one tenor** -- the property the two-file layout exists for.

Convention: full ``import x`` only. No ``from x import y``.
"""

import pathlib
import sqlite3

import pytest

import claude_worker.bin15_seed
import claude_worker.xsd_author
import claude_worker.vol_ref
import claude_worker.vrp_seed

_MINUTE_MS: int = 60_000
_DAY_MS: int = 86_400_000
#: 2026-09-12 06:00:00 UTC -- a native-daily expiry instant.
_NATIVE_MS: int = 1_789_192_800_000
#: 2026-09-12 06:00:00 UTC is also on the quarter, so it serves both.
_QUARTER_MS: int = _NATIVE_MS
_DESCRIPTOR: str = "hyperliquid:BTC"


def _make_db(path: pathlib.Path, descriptor: str, first_ms: int, closes: list[int]) -> None:
    """A minimal `candles` table carrying 1-minute closes."""
    conn = sqlite3.connect(path)
    try:
        conn.execute(
            "CREATE TABLE candles (venue TEXT, descriptor TEXT, tf TEXT,"
            " open_ts INTEGER, c REAL, PRIMARY KEY (venue, descriptor, tf, open_ts))"
        )
        conn.executemany(
            "INSERT INTO candles (venue, descriptor, tf, open_ts, c) VALUES (?,?,?,?,?)",
            [
                ("hyperliquid", descriptor, "1m", first_ms + i * _MINUTE_MS, c / 1_000_000)
                for i, c in enumerate(closes)
            ],
        )
        conn.commit()
    finally:
        conn.close()


def _tape(n: int, seed: int = 20260912) -> list[int]:
    """A deterministic integer walk in x1e6 dollars."""
    out: list[int] = []
    px = 77_177_000_000
    s = seed
    for _ in range(n):
        s = (s * 6364136223846793005 + 1442695040888963407) % (2**64)
        px += ((s >> 32) % 40_000_000) - 20_000_000
        px = max(px, 1_000_000_000)
        out.append(px)
    return out


def _db_covering(tmp_path: pathlib.Path, minutes: int, until_ms: int) -> pathlib.Path:
    """A db whose last close opens one minute before ``until_ms``."""
    first = until_ms - minutes * _MINUTE_MS
    db = tmp_path / "candles.db"
    _make_db(db, _DESCRIPTOR, first, _tape(minutes))
    return db


# --- the family grammar -------------------------------------------


def test_parse_family_accepts_two_forms_and_refuses_crossed_ones() -> None:
    out = claude_worker.bin15_seed.parse_family("out:BTC:15m")
    assert out.coin == "BTC"
    assert out.kind == claude_worker.bin15_seed.FAMILY_OUT_15M
    assert out.tau_ns == claude_worker.vol_ref.TAU_15M_NS
    native = claude_worker.bin15_seed.parse_family("native:HYPE:1d")
    assert native.coin == "HYPE"
    assert native.kind == claude_worker.bin15_seed.FAMILY_NATIVE_DAILY
    assert native.tau_ns == claude_worker.vol_ref.TAU_8H_NS
    # Crossed forms are the ones that would seed a daily off a 15 m
    # forecast; `bin15_boot::kind_of_key` refuses them too.
    for bad in ("out:BTC:1d", "native:BTC:15m", "out:BTC", "out:BTC:15m:x", "", "out::15m"):
        with pytest.raises(ValueError):
            claude_worker.bin15_seed.parse_family(bad)


def test_the_seed_name_is_where_bin15_boot_looks() -> None:
    assert (
        claude_worker.bin15_seed.parse_family("out:BTC:15m").seed_name() == "bin15-seed-BTC.tsv"
    )
    assert (
        claude_worker.bin15_seed.parse_family("native:BTC:1d").seed_name()
        == "bin15-seed-BTC-1d.tsv"
    )
    # The mark source is the one the member itself prices off.
    assert (
        claude_worker.bin15_seed.parse_family("out:SOL:15m").default_descriptor
        == "hyperliquid:SOL"
    )


# --- the expiry grids ----------------------------------------------


def test_quarter_hour_expiries_are_on_the_quarter_and_never_in_the_future() -> None:
    now = _QUARTER_MS + 7 * _MINUTE_MS  # seven minutes into an instance
    got = claude_worker.bin15_seed.quarter_hour_expiries_ms(now, 3)
    assert got == [_QUARTER_MS - 2 * 900_000, _QUARTER_MS - 900_000, _QUARTER_MS]
    assert all(ts <= now for ts in got)
    assert all(ts % 900_000 == 0 for ts in got)
    # Exactly on the quarter, that quarter HAS expired.
    assert claude_worker.bin15_seed.quarter_hour_expiries_ms(_QUARTER_MS, 1) == [_QUARTER_MS]


def test_native_expiries_are_0600_utc_and_never_in_the_future() -> None:
    now = _NATIVE_MS + 3 * 3_600_000  # 09:00 UTC
    got = claude_worker.bin15_seed.native_expiries_ms(now, 3)
    assert got == [_NATIVE_MS - 2 * _DAY_MS, _NATIVE_MS - _DAY_MS, _NATIVE_MS]
    # An hour BEFORE settle, today's expiry has not happened yet.
    earlier = claude_worker.bin15_seed.native_expiries_ms(_NATIVE_MS - 3_600_000, 2)
    assert earlier[-1] == _NATIVE_MS - _DAY_MS


def test_the_grid_follows_the_family_kind() -> None:
    fam15 = claude_worker.bin15_seed.parse_family("out:BTC:15m")
    fam1d = claude_worker.bin15_seed.parse_family("native:BTC:1d")
    now = _NATIVE_MS + 3_600_000
    assert claude_worker.bin15_seed.expiries_ms(fam15, now, 2)[-1] % 900_000 == 0
    assert claude_worker.bin15_seed.expiries_ms(fam1d, now, 2) == [
        _NATIVE_MS - _DAY_MS,
        _NATIVE_MS,
    ]


# --- the lookahead law ---------------------------------------------


def test_an_unsettled_expiry_is_never_emitted(tmp_path: pathlib.Path) -> None:
    """Inventing a ``y`` for a hold that has not finished is how a
    backtest of this member comes out profitable when it is not."""
    db = _db_covering(tmp_path, 1_600, _QUARTER_MS + 900_000)
    fam = claude_worker.bin15_seed.parse_family("out:BTC:15m")
    spec = claude_worker.bin15_seed.CutSpec(fam, _DESCRIPTOR)
    stats = claude_worker.vrp_seed.CutStats()
    conn = sqlite3.connect(db)
    try:
        # `now` is one minute BEFORE the expiry.
        row = claude_worker.bin15_seed.pair_for_expiry(
            conn, spec, _QUARTER_MS, _QUARTER_MS - _MINUTE_MS, stats
        )
    finally:
        conn.close()
    assert row is None
    assert stats.skipped_unsettled == 1
    assert stats.emitted == 0


def test_a_pair_is_formed_only_from_minutes_before_its_entry(tmp_path: pathlib.Path) -> None:
    """``x`` must not move when the HOLD's prices change.

    The engine is rebuilt per expiry from ``[entry - 1440 min, entry)``,
    so rewriting every close inside the hold may move ``y`` and must
    leave ``x`` exactly where it was.
    """
    minutes = 1_600
    first = _QUARTER_MS + 900_000 - minutes * _MINUTE_MS
    tape = _tape(minutes)
    fam = claude_worker.bin15_seed.parse_family("out:BTC:15m")
    spec = claude_worker.bin15_seed.CutSpec(fam, _DESCRIPTOR)
    now = _QUARTER_MS + 900_000

    def cut(closes: list[int]) -> claude_worker.vrp_seed.SeedRow:
        db = tmp_path / f"c{len(closes)}-{closes[-1]}.db"
        _make_db(db, _DESCRIPTOR, first, closes)
        conn = sqlite3.connect(db)
        try:
            row = claude_worker.bin15_seed.pair_for_expiry(
                conn, spec, _QUARTER_MS, now, claude_worker.vrp_seed.CutStats()
            )
        finally:
            conn.close()
        assert row is not None
        return row

    base = cut(list(tape))
    # Everything from the entry instant onward gets a violent move.
    entry_idx = (_QUARTER_MS - 900_000 - first) // _MINUTE_MS
    shocked = list(tape)
    for i in range(entry_idx, len(shocked)):
        shocked[i] = shocked[i] + 5_000_000_000
    after = cut(shocked)
    assert after.x_1e9 == base.x_1e9, "the hold leaked into the regressor"
    assert after.y_1e9 != base.y_1e9, "the shock must be visible in y"


def test_the_tenor_decides_the_pair(tmp_path: pathlib.Path) -> None:
    """The reason there are two seed files.

    The same expiry, cut for the 15 m family and for the daily family,
    gives DIFFERENT pairs -- the 15 m tenor folds four HAR windows and
    the 8 h tenor folds three, over different holds. One cloud pushed
    into both forecast engines fits the daily line on the 15 m
    regressor, and because both are log-vols of the same series the
    result looks plausible and is wrong.
    """
    db = _db_covering(tmp_path, 2_600, _NATIVE_MS + _MINUTE_MS)
    fam15 = claude_worker.bin15_seed.parse_family("out:BTC:15m")
    fam1d = claude_worker.bin15_seed.parse_family("native:BTC:1d")
    conn = sqlite3.connect(db)
    try:
        rows = [
            claude_worker.bin15_seed.pair_for_expiry(
                conn,
                claude_worker.bin15_seed.CutSpec(fam, _DESCRIPTOR),
                _NATIVE_MS,
                _NATIVE_MS + _MINUTE_MS,
                claude_worker.vrp_seed.CutStats(),
            )
            for fam in (fam15, fam1d)
        ]
    finally:
        conn.close()
    fifteen, daily = rows
    assert fifteen is not None and daily is not None
    assert fifteen.expiry_ts_ms == daily.expiry_ts_ms
    assert fifteen.x_1e9 != daily.x_1e9, "the two tenors must not share a regressor"
    assert fifteen.y_1e9 != daily.y_1e9, "nor a realised target"


# --- the file ------------------------------------------------------


def test_the_file_is_the_vrp_grammar_the_engine_already_parses(tmp_path: pathlib.Path) -> None:
    db = _db_covering(tmp_path, 1_700, _QUARTER_MS + 900_000)
    fam = claude_worker.bin15_seed.parse_family("out:BTC:15m")
    spec = claude_worker.bin15_seed.CutSpec(fam, _DESCRIPTOR, pairs=4)
    out = tmp_path / "bin15-seed-BTC.tsv"
    n, stats = claude_worker.bin15_seed.seed_out(db, spec, out, _QUARTER_MS + 900_000)
    text = out.read_text(encoding="utf-8")
    assert f"V\t{claude_worker.vrp_seed.SEED_VERSION}" in text, "the version row boot checks"
    p_rows = [ln for ln in text.splitlines() if ln.startswith("P\t")]
    r_rows = [ln for ln in text.splitlines() if ln.startswith("R\t")]
    assert len(p_rows) == n >= 1
    assert len(r_rows) == stats.window_minutes >= 1
    # `P expiry x y` and `R min_ts r`, tab-separated, strictly
    # increasing in their stamp -- the two things the engine's parsers
    # refuse a file for.
    assert all(len(ln.split("\t")) == 4 for ln in p_rows)
    assert all(len(ln.split("\t")) == 3 for ln in r_rows)
    stamps = [int(ln.split("\t")[1]) for ln in p_rows]
    assert stamps == sorted(set(stamps))
    stamps = [int(ln.split("\t")[1]) for ln in r_rows]
    assert stamps == sorted(set(stamps))
    assert not list(tmp_path.glob("*.tmp")), "the temp file must be renamed away"


def test_the_daily_file_carries_pairs_only(tmp_path: pathlib.Path) -> None:
    """The minute window has ONE source per underlying: the 15 m file.
    Two would replay the same minutes into a ring that assumes
    chronological order."""
    db = _db_covering(tmp_path, 2_600, _NATIVE_MS + _MINUTE_MS)
    fam = claude_worker.bin15_seed.parse_family("native:BTC:1d")
    spec = claude_worker.bin15_seed.CutSpec(fam, _DESCRIPTOR, pairs=2)
    out = tmp_path / "bin15-seed-BTC-1d.tsv"
    n, _ = claude_worker.bin15_seed.seed_out(db, spec, out, _NATIVE_MS + _MINUTE_MS)
    text = out.read_text(encoding="utf-8")
    assert n >= 1
    assert any(ln.startswith("P\t") for ln in text.splitlines())
    assert not any(ln.startswith("R\t") for ln in text.splitlines())
    # And it can be asked for explicitly.
    with_window = tmp_path / "explicit.tsv"
    claude_worker.bin15_seed.seed_out(
        db, spec, with_window, _NATIVE_MS + _MINUTE_MS, window=True
    )
    assert any(ln.startswith("R\t") for ln in with_window.read_text().splitlines())


def test_seed_all_writes_one_file_per_coin_and_tenor(tmp_path: pathlib.Path) -> None:
    db = _db_covering(tmp_path, 2_600, _NATIVE_MS + _MINUTE_MS)
    artifact = tmp_path / "bin15.toml"
    artifact.write_text(
        '[bin15]\n'
        'families = ["out:BTC:15m", "native:BTC:1d", "out:ETH:15m"]\n'
        'underlying = ["hyperliquid:BTC"]\n',
        encoding="utf-8",
    )
    written = claude_worker.bin15_seed.seed_all(
        db, artifact, tmp_path, pairs=2, now_ms=_NATIVE_MS + _MINUTE_MS
    )
    names = [name for name, _, _ in written]
    # ETH has no `underlying` row, so it is skipped rather than cut from
    # a series that is not its own.
    assert names == ["bin15-seed-BTC.tsv", "bin15-seed-BTC-1d.tsv"]
    for name in names:
        assert (tmp_path / name).is_file()


def test_seed_all_refuses_an_artifact_with_no_families(tmp_path: pathlib.Path) -> None:
    db = _db_covering(tmp_path, 1_600, _QUARTER_MS + 900_000)
    artifact = tmp_path / "bin15.toml"
    artifact.write_text('[bin15]\nunderlying = ["hyperliquid:BTC"]\n', encoding="utf-8")
    with pytest.raises(ValueError):
        claude_worker.bin15_seed.seed_all(db, artifact, tmp_path)


def test_a_short_history_is_counted_not_invented(tmp_path: pathlib.Path) -> None:
    """A db that cannot fill the 1 440-minute window yields no pair and
    says which gate stopped it."""
    db = _db_covering(tmp_path, 200, _QUARTER_MS + 900_000)
    fam = claude_worker.bin15_seed.parse_family("out:BTC:15m")
    spec = claude_worker.bin15_seed.CutSpec(fam, _DESCRIPTOR, pairs=4)
    out = tmp_path / "bin15-seed-BTC.tsv"
    n, stats = claude_worker.bin15_seed.seed_out(db, spec, out, _QUARTER_MS + 900_000)
    assert n == 0
    assert stats.skipped_short_history > 0
    assert stats.emitted == 0
    # The file is still written, and boot reads it as a cold tenor
    # rather than a malformed one.
    assert out.is_file()


def test_the_default_db_is_the_live_one_not_the_stray() -> None:
    """`~/multivenue/candles.db` is a 0-byte stray on this host; the
    real database lives under `worker/`. One spelling, shared with
    `xsd_author`, so the wrapper and the module cannot disagree."""
    assert claude_worker.bin15_seed.DEFAULT_DB == "~/multivenue/worker/candles.db"
    assert claude_worker.bin15_seed.DEFAULT_DB == claude_worker.xsd_author.DEFAULT_DB


def test_the_cli_cuts_a_file_and_refuses_a_bad_family(tmp_path: pathlib.Path) -> None:
    db = _db_covering(tmp_path, 1_700, _QUARTER_MS + 900_000)
    out = tmp_path / "bin15-seed-BTC.tsv"
    rc = claude_worker.bin15_seed.main(
        [
            "seed-out",
            "--db",
            str(db),
            "--family",
            "out:BTC:15m",
            "--out",
            str(out),
            "--pairs",
            "4",
            "--now-ms",
            str(_QUARTER_MS + 900_000),
        ]
    )
    assert rc == 0
    assert out.is_file()
    assert (
        claude_worker.bin15_seed.main(
            ["seed-out", "--db", str(db), "--family", "out:BTC:1d", "--out", str(out)]
        )
        == 2
    )
