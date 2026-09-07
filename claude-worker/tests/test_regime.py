# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""regime.py — the Python half of the Rust ↔ Python regime-law parity (RG1).

``tests/fixtures/regime/parity-<n>.{input,expected}.tsv`` are the SAME files
``crates/core-regime/tests/parity.rs`` consumes; the expected files are
written by the Rust law and asserted here line by line, so the two
implementations cannot drift without one suite going red. Fixture 1 = the
RG1 law (hysteresis keys absent); fixture 2 = the same tape under the RG7
hysteresis (per-profile confirm + the §3.5 exit bands).

Convention: full ``import x`` only. No ``from x import y``.
"""

import dataclasses
import pathlib

import pytest

import claude_worker.frames
import claude_worker.regime

_DIR: pathlib.Path = pathlib.Path(__file__).resolve().parent / "fixtures" / "regime"
_FIXTURES: tuple[str, ...] = ("parity-1", "parity-2")


@dataclasses.dataclass(slots=True)
class Input:
    params: claude_worker.regime.RegimeParams
    minute0: int
    closes: list[tuple[int, int, int]]
    funding: list[tuple[int, int, int]]
    declared: list[tuple[int, int, int, int]]


def _kv(parts: list[str], key: str) -> str:
    for p in parts:
        if p.startswith(key + "="):
            return p[len(key) + 1 :]
    raise KeyError(key)


def _kv_or(parts: list[str], key: str) -> int:
    """An optional ``key=value`` (the RG7 hysteresis keys; absent = 0)."""
    try:
        return int(_kv(parts, key))
    except KeyError:
        return 0


def _profile(parts: list[str]) -> claude_worker.regime.ProfileParams:
    g = lambda k: int(_kv(parts, k))  # noqa: E731 — tiny local accessor
    return claude_worker.regime.ProfileParams(
        g("trend_w"), g("shape_w"), g("vol_w"), g("stretch_w"), g("rel_w"), g("fund_prints"),
        g("trend_thr"), g("breadth_q"),
        g("er_lo_enter"), g("er_lo_exit"), g("er_hi_enter"), g("er_hi_exit"),
        g("rv_p30"), g("rv_p70"), g("stretch_k"), g("rel_thr"), g("fund_p30"), g("fund_p70"),
        _kv_or(parts, "confirm"), _kv_or(parts, "trend_exit"),
        _kv_or(parts, "rv_exit_frac"), _kv_or(parts, "stretch_exit_k"),
    )


def load_input(name: str = "parity-1") -> Input:
    btc = fund = confirm = minute0 = 0
    members: tuple[int, ...] = ()
    profiles: list[claude_worker.regime.ProfileParams] = []
    closes: list[tuple[int, int, int]] = []
    funding: list[tuple[int, int, int]] = []
    declared: list[tuple[int, int, int, int]] = []
    for raw in (_DIR / f"{name}.input.tsv").read_text("utf-8").splitlines():
        line = raw.strip()
        if not line or line.startswith("#"):
            continue
        parts = line.split()
        if parts[0] == "P" and parts[1].startswith("btc="):
            btc = int(_kv(parts, "btc"))
            fund = int(_kv(parts, "fund"))
            members = tuple(int(x) for x in _kv(parts, "members").split(","))
            confirm = int(_kv(parts, "confirm"))
        elif parts[0] == "P":
            profiles.append(_profile(parts))
        elif parts[0] == "T":
            minute0 = int(_kv(parts, "minute0"))
        elif parts[0] == "C":
            closes.append((int(parts[1]), int(parts[2]), int(parts[3])))
        elif parts[0] == "F":
            funding.append((int(parts[1]), int(parts[2]), int(parts[3])))
        elif parts[0] == "D":
            declared.append(
                (int(parts[1]), int(parts[2]), int(parts[3], 16), int(parts[4]) * claude_worker.regime.MINUTE_NS)
            )
        else:
            raise ValueError(f"unknown record {parts[0]}")
    params = claude_worker.regime.RegimeParams(btc, fund, members, confirm, tuple(profiles))
    return Input(params, minute0, closes, funding, declared)


def _fmt(v: int | None) -> str:
    return "-" if v is None else str(v)


def run(name: str = "parity-1") -> list[str]:
    inp = load_input(name)
    ev = claude_worker.regime.RegimeEvaluator(inp.params)
    minute_ns = claude_worker.regime.MINUTE_NS
    # Seed rows (minutes before the first live one) are plain closes;
    # the engine replays the last 2·max confirm minutes to warm its judges.
    for m, sym, c in inp.closes:
        if m < inp.minute0:
            assert ev.close(m, sym, c)
    for m in range(inp.minute0 - 2 * inp.params.max_confirm_min(), inp.minute0):
        ev.roll(m, inp.minute0 * minute_ns, count=False)
    last = max(m for m, _, _ in inp.closes)
    out: list[str] = []
    for m in range(inp.minute0, last + 1):
        start = m * minute_ns
        for fm, rate, ms in inp.funding:
            if fm == m:
                ev.funding(rate, ms)
        for dm, p, word, ttl in inp.declared:
            if dm == m:
                ev.set_declared(p, word, start, ttl)
        for cm, sym, c in inp.closes:
            if cm == m:
                ev.close(m, sym, c)
        # The engine's timer crosses the boundary 1 ms after it.
        ev.roll(m, start + minute_ns + 1_000_000)
        for p in range(claude_worker.regime.REGIME_PROFILES):
            raw = ev.raw[p]
            rel = ",".join(f"{ev.rel_of(p, s):02x}" for s in inp.params.members)
            flips = ",".join(str(ev.flips[p][d]) for d in range(6))
            out.append(
                f"E {m} {p} {claude_worker.regime.word_hex(ev.measured[p])} "
                f"{claude_worker.regime.word_hex(ev.effective[p])} "
                f"{_fmt(raw.ret_bps_1e9)} {_fmt(raw.er_1e9)} {_fmt(raw.rv_bps_1e9)} {_fmt(raw.stretch_1e9)} "
                f"{rel} {raw.breadth_up},{raw.breadth_dn},{raw.breadth_n} {flips} {ev.disagree[p]} {ev.minutes_judged}"
            )
    return out


@pytest.mark.parametrize("name", _FIXTURES)
def test_python_law_reproduces_the_rust_expected_file(name: str) -> None:
    got = run(name)
    want = [
        line.strip()
        for line in (_DIR / f"{name}.expected.tsv").read_text("utf-8").splitlines()
        if line.strip() and not line.startswith("#")
    ]
    assert len(got) == len(want), f"{name}: line count drifted"
    for i, (g, w) in enumerate(zip(got, want, strict=True)):
        assert g == w, f"{name} line {i}: python law drifted from the Rust expected file"


def test_hysteresis_fixture_flips_less_on_the_fast_profile() -> None:
    """RG7: fixture 2 is fixture 1's tape under the hysteresis keys — the
    fast profile must flip strictly less (the point of the fix)."""
    plain = run("parity-1")
    hyst = run("parity-2")

    def fast_flips(lines: list[str]) -> int:
        last = next(line for line in reversed(lines) if line.split()[2] == "0")
        return sum(int(v) for v in last.split()[-3].split(","))

    assert fast_flips(hyst) < fast_flips(plain)
    assert plain != hyst


def test_exit_bands_and_per_profile_confirm_mirror_the_engine() -> None:
    """The banded judges hold a committed state inside the band and are
    the pre-fix law bit for bit without one; a profile's own confirm wins."""
    r = claude_worker.regime
    plain = dataclasses.replace(
        r.FAST_DEFAULT, rv_p30_bps_1e9=10_000_000_000, rv_p70_bps_1e9=100_000_000_000
    )
    bull, bear, neutral = r.TREND_BULL, r.TREND_BEAR, r.TREND_NEUTRAL
    assert r.judge_trend(bull, 20_000_000_000, 0, 0, 3, 3, plain) == neutral
    assert r.judge_trend(bull, 40_000_000_000, 0, 0, 3, 3, plain) == neutral  # no breadth
    assert r.judge_vol(r.VOL_LOW, 10_000_000_000, plain) == r.VOL_NORMAL
    assert r.judge_stretch(r.STRETCH_EXT_UP, 1_900_000_000, plain) == r.STRETCH_NEUTRAL
    pp = dataclasses.replace(
        plain,
        trend_exit_bps_1e9=20_000_000_000,
        rv_exit_frac_1e9=100_000_000,
        stretch_exit_k_1e9=1_500_000_000,
    )
    assert r.judge_trend(bull, 25_000_000_000, 0, 0, 3, 3, pp) == bull
    assert r.judge_trend(bull, 20_000_000_000, 0, 0, 3, 3, pp) == neutral
    assert r.judge_trend(bear, -25_000_000_000, 0, 0, 3, 3, pp) == bear
    assert r.judge_trend(neutral, 31_000_000_000, 3, 0, 3, 3, pp) == bull
    assert r.judge_vol(r.VOL_LOW, 10_900_000_000, pp) == r.VOL_LOW
    assert r.judge_vol(r.VOL_LOW, 11_000_000_000, pp) == r.VOL_NORMAL
    assert r.judge_vol(r.VOL_HIGH, 90_100_000_000, pp) == r.VOL_HIGH
    assert r.judge_vol(r.VOL_HIGH, 90_000_000_000, pp) == r.VOL_NORMAL
    assert r.judge_stretch(r.STRETCH_EXT_UP, 1_600_000_000, pp) == r.STRETCH_EXT_UP
    assert r.judge_stretch(r.STRETCH_EXT_UP, 1_500_000_000, pp) == r.STRETCH_NEUTRAL
    params = r.RegimeParams(0, 0, (), 2, (dataclasses.replace(pp, confirm_min=5), pp))
    assert (params.confirm_of(0), params.confirm_of(1), params.max_confirm_min()) == (5, 2, 5)
    r.RegimeEvaluator(params)
    for bad in (
        {"trend_exit_bps_1e9": 30_000_000_001},
        {"rv_exit_frac_1e9": 1_000_000_001},
        {"stretch_exit_k_1e9": 2_000_000_001},
        {"confirm_min": 256},
    ):
        with pytest.raises(ValueError):
            r.RegimeEvaluator(r.RegimeParams(0, 0, (), 2, (dataclasses.replace(pp, **bad), pp)))


def test_math_primitives_match_the_rust_definitions() -> None:
    r = claude_worker.regime
    assert r.ret_bps_1e9(100_000_000, 101_000_000) == 100_000_000_000
    assert r.ret_bps_1e9(300_000_000, 299_999_999) == -33_334  # floored toward −∞
    assert r.ret_bps_1e9(0, 5) == 0
    assert r.isqrt_i128(17) == 4
    assert r.isqrt_i128(-1) == 0
    assert r.isqrt_i128((1 << 126)) == (1 << 63) - 1  # saturates like i64
    assert r.floor_div(-7, 2) == -4


def test_confirm_law() -> None:
    r = claude_worker.regime
    j = r.Judge()
    assert not j.feed(r.TREND_BULL, 3)
    assert not j.feed(r.TREND_BULL, 3)
    assert not j.feed(r.TREND_BEAR, 3)
    assert not j.feed(r.TREND_BULL, 3)
    assert not j.feed(r.TREND_BULL, 3)
    assert not j.feed(r.TREND_BULL, 3)  # ABSENT→BULL is not a flip
    assert j.cur == r.TREND_BULL
    assert not j.feed(r.TREND_BEAR, 3)
    assert not j.feed(r.TREND_BEAR, 3)
    assert j.feed(r.TREND_BEAR, 3)
    assert j.cur == r.TREND_BEAR


def test_shape_bands_and_merge() -> None:
    r = claude_worker.regime
    pp = r.FAST_DEFAULT
    assert r.judge_shape(r.SHAPE_CHOP, 320_000_000, pp) == r.SHAPE_CHOP
    assert r.judge_shape(r.SHAPE_MIXED, 320_000_000, pp) == r.SHAPE_MIXED
    assert r.judge_shape(r.SHAPE_TREND, 570_000_000, pp) == r.SHAPE_TREND
    assert r.judge_shape(r.ABSENT, 700_000_000, pp) == r.SHAPE_TREND
    m = claude_worker.frames.regime_word(
        trend="bull", shape="trend", vol="low", fund="pos", level="low", stretch="neutral", source="measured"
    )
    d = claude_worker.frames.regime_word(vol="high")
    e = r.merge_declared(d, m)
    assert claude_worker.frames.regime_word_dims(e)["vol"] == "high"
    assert claude_worker.frames.regime_word_dims(e)["trend"] == "bull"
    assert claude_worker.frames.regime_word_dims(e)["source"] == "declared"
    assert r.declared_disagrees(d, m)
    assert not r.declared_disagrees(0, m)
    assert r.any_known(m)
    assert not r.any_known(r.UNKNOWN_WORD)
    assert r.describe(r.UNKNOWN_WORD).endswith("source=unknown")
    assert r.describe(0) == "empty"
    assert r.word_hex(r.UNKNOWN_WORD) == "0004808080808080"


def test_evaluator_refuses_bad_params() -> None:
    r = claude_worker.regime
    good = r.RegimeParams(1, 1, (2, 3), 2, (r.FAST_DEFAULT, r.SLOW_DEFAULT))
    r.RegimeEvaluator(good)
    with pytest.raises(ValueError):
        r.RegimeEvaluator(dataclasses.replace(good, confirm_min=0))
    with pytest.raises(ValueError):
        r.RegimeEvaluator(dataclasses.replace(good, members=(2, 2)))
    with pytest.raises(ValueError):
        r.RegimeEvaluator(dataclasses.replace(good, members=(1,)))
    with pytest.raises(ValueError):
        r.RegimeEvaluator(dataclasses.replace(good, profiles=(r.FAST_DEFAULT,)))


# ---- RG2: the seed lane ----


def test_seed_out_exports_member_closes_as_integer_minutes(tmp_path: pathlib.Path) -> None:
    import claude_worker.candles

    r = claude_worker.regime
    regime_toml = tmp_path / "regime.toml"
    regime_toml.write_text(
        '[refs]\nbtc = "binance-usdm:btcusdt"\nfund = "binance-usdm:btcusdt"\n'
        '[breadth]\nmembers = ["binance-usdm:ethusdt"]\n[hysteresis]\nconfirm_min = 3\n'
        "[profile.fast]\ntrend_w_min = 60\n[profile.slow]\ntrend_w_min = 240\n",
        encoding="utf-8",
    )
    assert r.read_regime_descriptors(regime_toml) == ["binance-usdm:btcusdt", "binance-usdm:ethusdt"]
    db = tmp_path / "candles.db"
    conn = claude_worker.candles.open_db(db)
    now_ms = 1_800_000_000_000
    rows = []
    for k in range(10):
        open_ts = now_ms - (10 - k) * 60_000
        rows.append((1, "binance-usdm:btcusdt", "1m", open_ts, 100.0, 101.0, 99.0, 100.0 + k * 0.5, 1.0, "rest", now_ms))
        rows.append((1, "binance-usdm:ethusdt", "1m", open_ts, 3000.0, 3001.0, 2999.0, 3000.0 + k, 1.0, "rest", now_ms))
    # A foreign descriptor, an hourly bar and a NULL close are all skipped.
    rows.append((2, "okx:BTC-USDT", "1m", now_ms - 60_000, 1.0, 1.0, 1.0, 1.0, 1.0, "rest", now_ms))
    rows.append((1, "binance-usdm:btcusdt", "1h", now_ms - 3_600_000, 1.0, 1.0, 1.0, 1.0, 1.0, "rest", now_ms))
    rows.append((1, "binance-usdm:btcusdt", "1m", now_ms - 11 * 60_000, 1.0, 1.0, 1.0, None, 1.0, "capture", now_ms))
    conn.executemany(
        "INSERT INTO candles (venue,descriptor,tf,open_ts,o,h,l,c,v,source,fetched_ts) VALUES (?,?,?,?,?,?,?,?,?,?,?)",
        rows,
    )
    conn.commit()
    conn.close()
    out = tmp_path / "regime-seed.tsv"
    n_desc, n_rows = r.seed_out(regime_toml, db, out, minutes=1536, now_ms=now_ms + 30_000)
    assert (n_desc, n_rows) == (2, 20)
    lines = [line for line in out.read_text("utf-8").splitlines() if line and not line.startswith("#")]
    assert len(lines) == 20
    first = lines[0].split("\t")
    assert first[0] == "binance-usdm:btcusdt"
    assert int(first[1]) == (now_ms - 10 * 60_000) // 60_000
    assert int(first[2]) == 100_000_000
    last_btc = lines[9].split("\t")
    assert int(last_btc[2]) == 104_500_000
    # The accumulating minute is excluded: a close AT now's minute is not exported.
    conn = claude_worker.candles.open_db(db)
    conn.execute(
        "INSERT INTO candles (venue,descriptor,tf,open_ts,o,h,l,c,v,source,fetched_ts) VALUES (?,?,?,?,?,?,?,?,?,?,?)",
        (1, "binance-usdm:btcusdt", "1m", now_ms, 1.0, 1.0, 1.0, 5.0, 1.0, "rest", now_ms),
    )
    conn.commit()
    conn.close()
    _, n_rows = r.seed_out(regime_toml, db, out, minutes=1536, now_ms=now_ms + 30_000)
    assert n_rows == 20
    # The CLI lane: exit 0 with a summary; missing inputs exit 2.
    assert r.main(["seed-out", "--regime", str(regime_toml), "--out", str(out), "--db", str(db)]) == 0
    assert r.main(["seed-out", "--regime", str(tmp_path / "nope.toml"), "--out", str(out), "--db", str(db)]) == 2
    assert r.main(["seed-out", "--regime", str(regime_toml), "--out", str(out), "--db", str(tmp_path / "nope.db")]) == 2
