# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""VM2 V6 (§1.6): the per-instrument CHANNEL MAP — which data channels
each descriptor carries, as one generated TSV the research agent (and
any offline consumer) reads instead of re-deriving venue lore.

``caps_of_descriptor`` here is the PYTHON MIRROR of the Rust offline
string law ``ingress_ai::caps_of_descriptor`` (crates/ingress-ai/src/
ruleset.rs) — the documented-permissive law the validator's rule 10
charges rows against. The two implementations are pinned against the
same fixture table (tests/test_channel_map.py mirrors the Rust
``caps_of_descriptor_law`` cases); change EITHER side only with the
other in the same commit.

Documented-permissive means: the map says what the descriptor CLASS
can carry, not what this boot's engine had wired (e.g. depth on
okx/deribit non-options is class-true even when a boot masks a lane).

Module surface only — never a worker verb (M2-close law). Invoked by
hand or by the V8 runbook::

    python -m claude_worker.channel_map            # newest run, stdout+file
    python -m claude_worker.channel_map --out -    # stdout only

Output: ``~/multivenue/worker/channel-map.tsv`` — one line per
manifest descriptor: ``sym<TAB>descriptor<TAB>caps<TAB>channels``
where caps is the decimal bit mask and channels the human list.

Convention: full ``import x`` only.
"""

import argparse
import os
import pathlib
import sys

import claude_worker.features
import claude_worker.iv_digest

# The capability bits (ingress-ai; wire-stable).
CAP_PRICE: int = 1
CAP_FUNDING: int = 2
CAP_DEPTH: int = 4
CAP_OPT: int = 8

_CAP_NAMES: tuple[tuple[int, str], ...] = (
    (CAP_PRICE, "price"),
    (CAP_FUNDING, "funding"),
    (CAP_DEPTH, "depth"),
    (CAP_OPT, "opt_summary"),
)

DEFAULT_REPLAY_DIR: str = "~/multivenue/logs"
DEFAULT_OUT: str = "~/multivenue/worker/channel-map.tsv"


def caps_of_descriptor(desc: str) -> int:
    """Python mirror of ``ingress_ai::caps_of_descriptor`` — keep the
    two in lockstep (see module docs)."""
    if ":" not in desc:
        return CAP_PRICE  # bare PM token id
    venue, name = desc.split(":", 1)
    is_opt = name.endswith("-C") or name.endswith("-P")
    if venue == "binance-opt":
        return CAP_OPT | CAP_PRICE
    if venue == "deribit":
        if is_opt:
            return CAP_OPT | CAP_PRICE
        if name.endswith("-PERPETUAL"):
            return CAP_PRICE | CAP_FUNDING | CAP_DEPTH
        return CAP_PRICE | CAP_DEPTH
    if venue == "okx":
        if is_opt:
            return CAP_OPT
        if name.endswith("-SWAP"):
            return CAP_PRICE | CAP_FUNDING | CAP_DEPTH
        return CAP_PRICE | CAP_DEPTH
    if venue == "binance-usdm":
        return CAP_PRICE | CAP_FUNDING
    if venue == "bybit-linear":
        return CAP_PRICE | CAP_FUNDING
    if venue == "hyperliquid":
        if name.startswith("#"):
            return CAP_PRICE
        return CAP_PRICE | CAP_FUNDING
    return CAP_PRICE


def channel_names(caps: int) -> str:
    """Human channel list for one caps mask (``+``-joined, bit
    order)."""
    return "+".join(name for bit, name in _CAP_NAMES if caps & bit)


def render_map(manifest: dict[tuple[int, int], str]) -> list[str]:
    """Manifest → TSV lines, sym-ascending (stable across
    invocations of the same run)."""
    rows = sorted(manifest.items(), key=lambda item: item[0][1])
    out: list[str] = []
    for (_venue, sym), desc in rows:
        caps = caps_of_descriptor(desc)
        out.append(f"{sym}\t{desc}\t{caps}\t{channel_names(caps)}")
    return out


def main(argv: list[str] | None = None) -> int:
    """CLI shim (module surface only — never a worker verb)."""
    parser = argparse.ArgumentParser(prog="claude_worker.channel_map")
    parser.add_argument("--replay-dir", default=None)
    parser.add_argument(
        "--out",
        default=None,
        help="output TSV path; '-' = stdout only",
    )
    args = parser.parse_args(argv)
    env = os.environ
    replay_root = pathlib.Path(
        args.replay_dir or env.get("CLAUDE_WORKER_REPLAY_DIR", "") or DEFAULT_REPLAY_DIR
    ).expanduser()

    run_dir = claude_worker.features.latest_run_dir(replay_root)
    if run_dir is None:
        print(f"channel-map: no run dirs under {replay_root}", file=sys.stderr)
        return 1
    manifest = claude_worker.iv_digest.read_manifest(run_dir)
    if manifest is None:
        print(f"channel-map: {run_dir.name}: no instrument manifest", file=sys.stderr)
        return 1
    lines = render_map(manifest[0])
    body = "\n".join(lines) + "\n"
    if args.out == "-":
        sys.stdout.write(body)
    else:
        out_path = pathlib.Path(args.out or DEFAULT_OUT).expanduser()
        out_path.parent.mkdir(parents=True, exist_ok=True)
        out_path.write_text(body, encoding="utf-8")
        print(
            f"channel-map: {len(lines)} descriptors from {run_dir.name}"
            f" -> {out_path}",
            file=sys.stderr,
        )
    if manifest[1]:
        print(
            f"channel-map: {manifest[1]} malformed manifest lines skipped",
            file=sys.stderr,
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
