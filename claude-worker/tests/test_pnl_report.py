# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""pnl_report module (M4.3) + the D1-unfrozen thin ``pnl`` verb.

Module: injectable-runner contract (argv shape, schema refusals,
report-pair writing, per-day idempotence, loud failure). Verb: thin
file reader only — latest/dated selection, summary-vs-json, exit 2
when nothing exists. One PATH-gated real-binary end-to-end rides the
same skip law as ``test_backtest_real``.

Convention: full ``import x`` only. No ``from x import y``.
"""

import datetime
import json
import pathlib
import shutil
import struct

import pytest
import typer.testing

import tests.craft

import claude_worker.backtest
import claude_worker.cli
import claude_worker.pmlr
import claude_worker.pnl_report

_RUNNER = typer.testing.CliRunner()

GOOD_JSON = (
    '{"audit_pnl_version":1,"runs":2,"paper":{"fills":0,"net_usd":"0.0"},'
    '"strategies":[{"strategy_id":0,"label":"latency-arb"}],'
    '"vm_by_ruleset":[],"vm_orders_no_hash":0}'
)
NOW_MS = 1_787_500_000_000  # fixed test clock


def day_of(now_ms: int) -> str:
    return datetime.datetime.fromtimestamp(
        now_ms / 1000, tz=datetime.timezone.utc
    ).strftime("%Y-%m-%d")


def ok_run_fn(argv: list[str]) -> tuple[int, str, str]:
    # The §14 spawn contract: PATH-resolved name + the audit-pnl argv.
    assert argv[0] == claude_worker.backtest.ENGINE_BINARY
    assert argv[1] == "audit-pnl" and argv[2] == "--dir"
    return 0, GOOD_JSON + "\n", "audit-pnl: summary line\n"


def run_once(tmp_path: pathlib.Path, run_fn) -> tuple[int, list[str], pathlib.Path]:
    lines: list[str] = []
    reports = tmp_path / "reports"
    rc = claude_worker.pnl_report.run_once(
        tmp_path / "logs", reports, NOW_MS, lines.append, run_fn=run_fn
    )
    return rc, lines, reports


# ---- module ---------------------------------------------------------------


def test_run_once_writes_the_report_pair(tmp_path):
    rc, lines, reports = run_once(tmp_path, ok_run_fn)
    assert rc == 0
    day = day_of(NOW_MS)
    json_path = reports / f"pnl-{day}.json"
    summary_path = reports / f"pnl-{day}.summary.txt"
    assert json.loads(json_path.read_text())["audit_pnl_version"] == 1
    assert summary_path.read_text() == "audit-pnl: summary line\n"
    assert any("strategies=1" in l and "runs=2" in l for l in lines)


RUN_JSON = (
    '{"audit_pnl_version":1,"runs":1,"window":{"wall_first_ns":%d,"wall_last_ns":%d,"utc_days":1},'
    '"paper":{"fills":0,"net_usd":"0.0"},'
    '"strategies":[{"strategy_id":6,"label":"icdp","orders":%d,"fills":%d,"trades":%d,'
    '"trading_days":1,"net_usd":"%s","realized_usd":"0.0","fees_usd":"0.01",'
    '"markout_usd":"0.0","max_drawdown_usd":"%s","canceled_end":0,"rejected_caps":0,'
    '"unroutable":0,"ioc_fills":%d,"ioc_canceled":1,"ttl_expired":0,'
    '"fee_ladder_net_usd":["1.0","0.5","0.0"],"per_day_net_usd":[{"day":0,"net_usd":"%s"}]}],'
    '"vm_by_ruleset":[{"hash128":"ab","orders":2,"trades":1,"net_usd":"0.25","max_drawdown_usd":"0.5"}],'
    '"vm_orders_no_hash":0}'
)


def _day_run_json(epoch_ns: int, orders: int, net: str, dd: str) -> str:
    return RUN_JSON % (epoch_ns, epoch_ns + 10, orders, orders // 2, orders // 2, net, dd, orders // 2, net)


def test_day_mode_audits_each_run_of_the_closed_day_with_the_fee_tier_and_merges(tmp_path):
    # ICDP I6: the nightly lane — per-run bounded audits, fee tier from
    # fees.toml, merge = sums + worst-run drawdown, failures listed.
    day_ns = 86_400 * 10**9
    d0 = (NOW_MS // 1000 // 86_400 - 1) * day_ns  # the closed UTC day, ns
    logs = tmp_path / "logs"
    runs = [d0 + 3_600 * 10**9, d0 + 8 * 3_600 * 10**9, d0 + 16 * 3_600 * 10**9]
    for e in runs:
        (logs / f"run-{e}").mkdir(parents=True)
    (logs / f"run-{d0 + day_ns + 60 * 10**9}").mkdir()  # today: excluded
    (logs / "not-a-run").mkdir()
    fees = tmp_path / "fees.toml"
    fees.write_text('# tier\n[fees]\npm = "0:0"\nbn = "2:5"  # perp retail\nokx = "2:5"\n')
    flags = claude_worker.pnl_report.load_fee_flags(fees)
    assert flags == ["--fee-bps", "pm:0:0", "--fee-bps", "bn:2:5", "--fee-bps", "okx:2:5"]
    seen: list[list[str]] = []

    def fn(argv):
        seen.append(argv)
        assert argv[0] == claude_worker.backtest.ENGINE_BINARY
        assert argv[1:3] == ["audit-pnl", "--dir"]
        assert argv[4:] == flags
        run = pathlib.Path(argv[3]).name
        if run == f"run-{runs[1]}":
            return 1, "", "boom\n"
        e = int(run[4:])
        return 0, _day_run_json(e, 4, "1.5", "0.75") + "\n", f"audit-pnl: {run}: stale: pm=1/3 (0bps)\n"

    lines: list[str] = []
    day = datetime.datetime.fromtimestamp(d0 / 1e9, tz=datetime.timezone.utc).strftime("%Y-%m-%d")
    rc = claude_worker.pnl_report.run_day(logs, tmp_path / "reports", day, lines.append, run_fn=fn, fee_flags=flags)
    assert rc == 0, lines
    assert [pathlib.Path(a[3]).name for a in seen] == [f"run-{e}" for e in runs]
    obj = json.loads((tmp_path / "reports" / f"pnl-{day}.json").read_text())
    assert obj["audit_pnl_version"] == 1 and obj["day"] == day and obj["runs"] == 2
    assert obj["failed_runs"] == [f"run-{runs[1]}"]
    row = obj["strategies"][0]
    assert row["strategy_id"] == 6 and row["label"] == "icdp" and row["runs"] == 2
    assert row["orders"] == 8 and row["fills"] == 4 and row["ioc_fills"] == 4 and row["ioc_canceled"] == 2
    assert row["net_usd"] == "3.000000" and row["fees_usd"] == "0.020000"
    assert row["max_drawdown_usd"] == "0.750000", "worst single run, not a sum"
    assert row["fee_ladder_net_usd"] == ["2.000000", "1.000000", "0.000000"]
    assert obj["vm_by_ruleset"] == [{"hash128": "ab", "orders": 4, "trades": 2, "net_usd": "0.500000", "max_drawdown_usd": "0.500000"}]
    assert obj["window"]["wall_first_ns"] == runs[0] and obj["window"]["wall_last_ns"] == runs[2] + 10
    assert len(obj["runs_detail"]) == 2
    summary = (tmp_path / "reports" / f"pnl-{day}.summary.txt").read_text()
    assert "fee tier flags: --fee-bps pm:0:0" in summary
    assert "FAILED (exit 1)" in summary and "stale: pm=1/3" in summary
    assert any("runs=2 failed=1" in l for l in lines)
    # Nothing for the day ⇒ loud nonzero.
    assert claude_worker.pnl_report.run_day(logs, tmp_path / "reports", "1999-01-01", lines.append, run_fn=fn) == 1
    # A malformed tier line is fatal.
    fees.write_text("[fees]\nbn = 2\n")
    with pytest.raises(ValueError):
        claude_worker.pnl_report.load_fee_flags(fees)


def _regime_section(mode: str, fast_words: list[tuple[str, str, int, list[dict]]]) -> dict:
    return {
        "mode": mode, "artifact_sha256": "ab" * 32, "seed_rows": 7680, "minutes_judged": sum(m for _, _, m, _ in fast_words),
        "declared_applied": 1 if mode == "artifact" else 0, "funding_events": 3,
        "set_regime_frames": 2, "set_regime_expired": 1,
        "profiles": [
            {"profile": "fast", "words": [{"word": w, "bits": b, "minutes": m, "strategies": s} for w, b, m, s in fast_words]},
            {"profile": "slow", "words": []},
        ],
    }


def _regime_row(sid: int, label: str, fills: int, net: str, ladder: list[str]) -> dict:
    return {"strategy_id": sid, "label": label, "orders": fills * 2, "fills": fills, "trades": fills // 2,
            "net_usd": net, "fee_ladder_net_usd": ladder}


def test_merge_folds_the_per_regime_section_across_runs_and_tolerates_pre_rg3_reports():
    # RG5 (plan §5.1): per (profile, word) minutes + strategy rows sum
    # across runs keyed by the word's bits; a run without the section
    # (pre-RG3 harness) contributes nothing; the summary head names each
    # regime the day was in with every strategy's fills / net / ladder.
    bull = ("trend=bull shape=trend", "0000000000000104", 90, [_regime_row(5, "vm", 4, "1.50", ["2.0", "1.0", "0.5"])])
    bear = ("trend=bear shape=chop", "0000000000000201", 30, [_regime_row(5, "vm", 1, "-0.25", ["0.0", "-0.1", "-0.2"]),
                                                             _regime_row(6, "icdp", 2, "0.10", ["0.2", "0.1", "0.0"])])
    bull2 = ("trend=bull shape=trend", "0000000000000104", 20, [_regime_row(5, "vm", 2, "0.50", ["1.0", "0.5", "0.0"])])
    base = json.loads(_day_run_json(1, 4, "1.5", "0.75"))
    r1 = {**base, "regime": _regime_section("artifact", [bull, bear])}
    r2 = {**base, "regime": _regime_section("artifact", [bull2])}
    r3 = {**base, "regime": _regime_section("blind", [])}
    r4 = dict(base)  # pre-RG3: no section at all
    merged = claude_worker.pnl_report.merge_reports("2026-09-05", [("run-1", r1), ("run-2", r2), ("run-3", r3), ("run-4", r4)])
    reg = merged["regime"]
    assert reg["modes"] == {"artifact": 2, "blind": 1}
    assert reg["minutes_judged"] == 140 and reg["declared_applied"] == 2
    assert reg["set_regime_frames"] == 6 and reg["set_regime_expired"] == 3
    assert [p["profile"] for p in reg["profiles"]] == ["fast", "slow"]
    fast = reg["profiles"][0]["words"]
    assert [(w["word"], w["minutes"]) for w in fast] == [("trend=bull shape=trend", 110), ("trend=bear shape=chop", 30)]
    vm_bull = fast[0]["strategies"]
    assert vm_bull == [{"strategy_id": 5, "label": "vm", "orders": 12, "fills": 6, "trades": 3,
                        "net_usd": "2.000000", "fee_ladder_net_usd": ["3.000000", "1.500000", "0.500000"]}]
    assert [s["strategy_id"] for s in fast[1]["strategies"]] == [5, 6]
    assert reg["profiles"][1]["words"] == []
    head = claude_worker.pnl_report.regime_head_lines(merged)
    assert head[0].startswith("regime: modes artifact=2, blind=1 minutes_judged=140")
    assert "regime fast [trend=bull shape=trend] minutes=110: vm[fills=6 net=2.000000" in head[1]
    assert "icdp[fills=2" in head[2] and len(head) == 3
    # Only pre-RG3 reports ⇒ an empty section and no head lines.
    empty = claude_worker.pnl_report.merge_reports("2026-09-05", [("run-4", r4)])
    assert empty["regime"] == {"modes": {}, "minutes_judged": 0, "declared_applied": 0,
                               "set_regime_frames": 0, "set_regime_expired": 0, "profiles": []}
    assert claude_worker.pnl_report.regime_head_lines(empty) == []


def test_day_mode_audits_two_hour_windows_and_cleans_the_cuts(tmp_path):
    # ICDP I6 + the 2 h law: a 3 h run becomes two window units, each a
    # bounded run dir of its own (epoch advanced), deleted after audit.
    day_ns = 86_400 * 10**9
    d0 = (NOW_MS // 1000 // 86_400 - 1) * day_ns
    epoch = d0 + 3_600 * 10**9
    logs = tmp_path / "logs"
    run = tests.craft.write_run(logs, epoch, [1_000, 1_000 + 3_600 * 10**9, 1_000 + 3 * 3_600 * 10**9 - 1])
    (run / "instrument-manifest.tsv").write_text("42\tPMTOK\n")
    seen: list[tuple[str, int]] = []

    def fn(argv):
        d = pathlib.Path(argv[3])
        n = len(list(claude_worker.pmlr.Reader(d / "pm-ticks.pmlr").ticks()))
        seen.append((d.name, n))
        assert (d / "instrument-manifest.tsv").is_file()
        return 0, _day_run_json(int(d.name[4:]), 2, "0.5", "0.1") + "\n", ""

    lines: list[str] = []
    day = datetime.datetime.fromtimestamp(d0 / 1e9, tz=datetime.timezone.utc).strftime("%Y-%m-%d")
    root = tmp_path / "nightly"
    rc = claude_worker.pnl_report.run_day(logs, tmp_path / "reports", day, lines.append, run_fn=fn, window_root=root)
    assert rc == 0, lines
    assert seen == [(f"run-{epoch}", 2), (f"run-{epoch + 7_200 * 10**9}", 1)]
    assert not any(root.glob("run-*")), "window cuts are deleted after their audit"
    obj = json.loads((tmp_path / "reports" / f"pnl-{day}.json").read_text())
    assert obj["runs"] == 2
    assert [r["run"] for r in obj["runs_detail"]] == [f"run-{epoch}@0s", f"run-{epoch}@7200s"]
    assert obj["strategies"][0]["orders"] == 4


def test_main_closed_day_selects_yesterday(tmp_path, monkeypatch):
    calls: list[list[str]] = []

    def fn(argv):
        calls.append(argv)
        e = int(pathlib.Path(argv[3]).name[4:])
        return 0, _day_run_json(e, 2, "0.1", "0.0") + "\n", ""

    monkeypatch.setattr(claude_worker.pnl_report, "_default_run_fn", fn)
    # hermetic: the operator's real ~/multivenue/fees.toml must not leak in
    monkeypatch.setattr(claude_worker.pnl_report, "FEES_PATH_DEFAULT", str(tmp_path / "no-fees.toml"))
    day_ns = 86_400 * 10**9
    yesterday = (NOW_MS // 1000 // 86_400 - 1) * day_ns + 5 * 10**9
    (tmp_path / "logs" / f"run-{yesterday}").mkdir(parents=True)
    # --fees pointing at an absent file is a loud error, not a silent 0/0
    with pytest.raises(FileNotFoundError):
        claude_worker.pnl_report.main([
            "--replay-dir", str(tmp_path / "logs"), "--reports-dir", str(tmp_path / "reports"),
            "--now-ms", str(NOW_MS), "--closed-day", "--fees", str(tmp_path / "absent.toml"),
        ])
    rc = claude_worker.pnl_report.main([
        "--replay-dir", str(tmp_path / "logs"), "--reports-dir", str(tmp_path / "reports"),
        "--now-ms", str(NOW_MS), "--closed-day",
    ])
    assert rc == 0
    assert len(calls) == 1 and calls[0][3].endswith(f"run-{yesterday}")
    day = datetime.datetime.fromtimestamp(yesterday / 1e9, tz=datetime.timezone.utc).strftime("%Y-%m-%d")
    assert (tmp_path / "reports" / f"pnl-{day}.json").is_file()


def test_run_once_is_idempotent_per_day_and_refreshes(tmp_path):
    rc, _, reports = run_once(tmp_path, ok_run_fn)
    assert rc == 0

    def fresh(argv):
        return 0, GOOD_JSON.replace('"runs":2', '"runs":3') + "\n", "s2\n"

    rc2, _, _ = run_once(tmp_path, fresh)
    assert rc2 == 0
    day = day_of(NOW_MS)
    assert len(list(reports.glob("pnl-*.json"))) == 1, "same-day pair refreshed"
    assert json.loads((reports / f"pnl-{day}.json").read_text())["runs"] == 3
    assert (reports / f"pnl-{day}.summary.txt").read_text() == "s2\n"


def test_run_once_fails_loudly_on_nonzero_exit(tmp_path):
    rc, lines, reports = run_once(tmp_path, lambda argv: (1, "", "boom\n"))
    assert rc == 1
    assert not (reports / f"pnl-{day_of(NOW_MS)}.json").exists()
    assert any("exited 1" in l for l in lines)


def test_run_once_refuses_empty_or_bad_or_wrong_version_stdout(tmp_path):
    for out in ("", "not json", '{"audit_pnl_version":2}'):
        rc, _, reports = run_once(tmp_path, lambda argv, o=out: (0, o, ""))
        assert rc == 1
        assert not (reports / f"pnl-{day_of(NOW_MS)}.json").exists()


# ---- the thin verb --------------------------------------------------------


def seed_reports(tmp_path: pathlib.Path) -> pathlib.Path:
    reports = tmp_path / "reports"
    reports.mkdir(parents=True)
    (reports / "pnl-2026-08-22.json").write_text(GOOD_JSON + "\n")
    (reports / "pnl-2026-08-22.summary.txt").write_text("day22 summary\n")
    (reports / "pnl-2026-08-23.json").write_text(
        GOOD_JSON.replace('"runs":2', '"runs":9') + "\n"
    )
    return reports


def invoke(reports: pathlib.Path, args: list[str]):
    return _RUNNER.invoke(
        claude_worker.cli.app,
        args,
        env={claude_worker.pnl_report.REPORTS_DIR_ENV: str(reports)},
    )


def test_pnl_verb_prints_latest_falling_back_to_json(tmp_path):
    reports = seed_reports(tmp_path)
    # Newest (08-23) has no summary file -> raw JSON fallback.
    res = invoke(reports, ["pnl"])
    assert res.exit_code == 0
    assert '"runs":9' in res.stdout


def test_pnl_verb_dated_summary_and_json_modes(tmp_path):
    reports = seed_reports(tmp_path)
    res = invoke(reports, ["pnl", "--date", "2026-08-22"])
    assert res.exit_code == 0
    assert "day22 summary" in res.stdout
    res = invoke(reports, ["pnl", "--date", "2026-08-22", "--json"])
    assert res.exit_code == 0
    assert '"runs":2' in res.stdout


def test_pnl_verb_missing_report_is_usage_error(tmp_path):
    reports = seed_reports(tmp_path)
    res = invoke(reports, ["pnl", "--date", "2026-01-01"])
    assert res.exit_code == claude_worker.cli.EXIT_USAGE
    empty = tmp_path / "empty"
    empty.mkdir()
    res = invoke(empty, ["pnl"])
    assert res.exit_code == claude_worker.cli.EXIT_USAGE


# ---- real-binary end-to-end (the test_backtest_real skip law) -------------

_ORDER = struct.Struct("<QIBB2xqqQBB")


def write_orders(path: pathlib.Path, epoch_ns: int, rows: list[tuple]) -> None:
    """v2 kind-3 file: rows = (ts, sym, side, px, qty, oid, strategy)."""
    header = claude_worker.pmlr._HEADER.pack(  # noqa: SLF001 — reader-defined layout, deliberately
        claude_worker.pmlr.MAGIC, 2, claude_worker.pmlr.SLOT_KIND_ORDER, epoch_ns
    )
    blob = bytearray(header + bytes(claude_worker.pmlr.HEADER_SIZE - len(header)))
    for ts, sym, side, px, qty, oid, strategy in rows:
        slot = _ORDER.pack(ts, sym, side, 0, px, qty, oid, 0, strategy)
        blob.extend(slot + bytes(claude_worker.pmlr.SLOT_SIZE - len(slot)))
    path.write_bytes(bytes(blob))


@pytest.mark.skipif(
    shutil.which("multivenue-engine") is None,
    reason="multivenue-engine release binary not on PATH (docs/local-setup.md runbook)",
)
def test_real_binary_end_to_end(tmp_path):
    epoch = 1_700_000_000_000_000_000
    run_dir = tmp_path / "logs" / f"run-{epoch}"
    run_dir.mkdir(parents=True)
    (run_dir / "instrument-manifest.tsv").write_text("42\tPMTOK\n")
    # Anchor + a post-Δ crossing tick (craft px 400k/420k; Δpm 200 ms).
    tests.craft.write_ticks(
        run_dir / "pm-ticks.pmlr", [1_000, 300_000_000], epoch
    )
    write_orders(
        run_dir / "engine-orders.pmlr",
        epoch,
        [(2_000, 42, 0, 500_000, 1_000_000, 7, 0)],
    )
    rc = claude_worker.pnl_report.main(
        [
            "--replay-dir", str(tmp_path / "logs"),
            "--reports-dir", str(tmp_path / "reports"),
            "--now-ms", str(NOW_MS),
        ]
    )
    assert rc == 0
    body = (tmp_path / "reports" / f"pnl-{day_of(NOW_MS)}.json").read_text()
    obj = json.loads(body)
    assert obj["audit_pnl_version"] == 1
    assert obj["strategies"][0]["strategy_id"] == 0
    assert obj["strategies"][0]["fills"] == 1


def test_latest_report_regimes_reads_the_merged_profiles(tmp_path):
    reports = tmp_path / "reports"
    assert claude_worker.pnl_report.latest_report_regimes(reports) == []
    reports.mkdir()
    (reports / "pnl-2026-09-04.json").write_text('{"audit_pnl_version":1}\n')
    assert claude_worker.pnl_report.latest_report_regimes(reports) == [], "pre-RG5 report: no section"
    (reports / "pnl-2026-09-05.json").write_text(
        json.dumps({"audit_pnl_version": 1, "regime": {"profiles": [{"profile": "fast", "words": []}, "junk"]}})
    )
    assert claude_worker.pnl_report.latest_report_regimes(reports) == [{"profile": "fast", "words": []}]
    (reports / "pnl-2026-09-06.json").write_text("{not json")
    assert claude_worker.pnl_report.latest_report_regimes(reports) == []
