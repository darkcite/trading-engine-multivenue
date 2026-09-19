# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""News & event intelligence lane — package root (NEWS spec §3).

Offline worker module (design §5): MAY allocate, never on the hot path.
Convention: full ``import x`` only. No ``from x import y``.

This package aggregates free news/structural sources, filters them, runs a
Haiku -> Sonnet -> Opus cascade over what survives, and turns the survivors
into AI command frames under an operator policy. Nothing here is ever
reached by the engine loop: the engine sees only 64-byte AI command frames
over the UDS ingress, exactly as the regime lane does.

Path doctrine (spec §3): ``BaseConfig`` is a frozen dataclass whose
``ServeConfig`` subclass adds a non-default field, so new fields there would
need ``kw_only`` and would ripple through every ``_cfg`` test helper. The
lane therefore resolves its own paths the way ``dashboard.Inputs`` does —
one frozen, slotted record built once from the environment, so a test points
every path at a tmp dir and never touches the operator's files.

Degraded modes are honest no-ops, never raises (spec §3, the
``regime-cycle`` law): an absent ``news.toml`` means every lane exits 0
having done nothing; an absent ``news-policy.toml`` means every action mode
is ``off``.
"""

import dataclasses
import os
import pathlib
import typing

import claude_worker.candles
import claude_worker.pnl_report
import claude_worker.regime

#: Bumped when the on-disk artifact shapes change (news.db DDL, the JSON
#: files under ``news_dir``). Written into every file output's ``"v"``.
SCHEMA_VERSION: int = 1

TOML_ENV: str = "NEWS_TOML"
DEFAULT_TOML: str = "~/multivenue/news.toml"
POLICY_TOML_ENV: str = "NEWS_POLICY_TOML"
DEFAULT_POLICY_TOML: str = "~/multivenue/news-policy.toml"
LLM_TOML_ENV: str = "NEWS_LLM_TOML"
DEFAULT_LLM_TOML: str = "~/multivenue/llm.toml"
NEWS_DIR_ENV: str = "CLAUDE_WORKER_NEWS_DIR"
DEFAULT_NEWS_DIR: str = "~/multivenue/worker/news"
STATE_DB_ENV: str = "CLAUDE_WORKER_DB"
DEFAULT_STATE_DB: str = "~/multivenue/worker/state.db"
MARKET_MAP_ENV: str = "CLAUDE_WORKER_MARKET_MAP"
DEFAULT_MARKET_MAP: str = "~/multivenue/worker/market-map.json"
MULTIVENUE_DIR_ENV: str = "CLAUDE_WORKER_MULTIVENUE_DIR"
DEFAULT_MULTIVENUE_DIR: str = "~/multivenue"

#: ``news.db`` lives beside the lane's other outputs, not beside ``state.db``
#: (spec Q1: a third store, its own retention, its own writers).
DB_FILENAME: str = "news.db"

#: File outputs under ``news_dir`` (spec §4.4). Named here so every writer
#: and every reader agrees without repeating a literal.
CALENDAR_FILE: str = "calendar.json"
SCORECARD_FILE: str = "scorecard.json"
ALERT_FILE: str = "ALERT"
UNIVERSE_PROPOSALS_FILE: str = "universe-proposals.toml"
XSD_PROPOSALS_FILE: str = "xsd-table-proposals.tsv"


@dataclasses.dataclass(frozen=True, slots=True)
class NewsPaths:
    """Every path and URL the lane reads or writes, resolved once.

    ``state_db_path`` is the CONTROL plane (``State``: the seq allocator and
    the prompt cache) and stays exactly where every other lane keeps it;
    ``db_path`` is this lane's own ``news.db``. They are opened side by side
    and never merged — the spec adds no table to ``state.db``.
    """

    toml_path: pathlib.Path
    policy_path: pathlib.Path
    #: The local-model sidecar's artifact (doc 03 §7). Absent means no local
    #: tiers, which is the same honest no-op an absent `news.toml` gives the
    #: whole lane.
    llm_path: pathlib.Path
    news_dir: pathlib.Path
    db_path: pathlib.Path
    state_db_path: pathlib.Path
    market_map_path: pathlib.Path
    replay_dir: pathlib.Path
    multivenue_dir: pathlib.Path
    candles_db_path: pathlib.Path
    regime_dir: pathlib.Path
    metrics_url: str

    def file(self, name: str) -> pathlib.Path:
        """One of the §4.4 outputs under ``news_dir``."""
        return self.news_dir / name


def _path_from(
    env: typing.Mapping[str, str],
    key: str,
    default: str,
) -> pathlib.Path:
    """``env[key]`` or ``default``, user-expanded (the ``config._path_from``
    contract: an empty value is an absent value)."""
    return pathlib.Path(env.get(key, "") or default).expanduser()


def paths_from_env(env: typing.Mapping[str, str] | None = None) -> NewsPaths:
    """Resolve [`NewsPaths`] from the environment (operator defaults).

    Nothing is created, opened or validated here — this is pure name
    resolution, so it is safe to call before deciding a lane is a no-op.
    """
    source: typing.Mapping[str, str] = os.environ if env is None else env
    news_dir = _path_from(source, NEWS_DIR_ENV, DEFAULT_NEWS_DIR)
    state_db = _path_from(source, STATE_DB_ENV, DEFAULT_STATE_DB)
    return NewsPaths(
        toml_path=_path_from(source, TOML_ENV, DEFAULT_TOML),
        policy_path=_path_from(source, POLICY_TOML_ENV, DEFAULT_POLICY_TOML),
        llm_path=_path_from(source, LLM_TOML_ENV, DEFAULT_LLM_TOML),
        news_dir=news_dir,
        db_path=news_dir / DB_FILENAME,
        state_db_path=state_db,
        market_map_path=_path_from(source, MARKET_MAP_ENV, DEFAULT_MARKET_MAP),
        replay_dir=claude_worker.pnl_report.resolve_replay_dir(source),
        multivenue_dir=_path_from(source, MULTIVENUE_DIR_ENV, DEFAULT_MULTIVENUE_DIR),
        candles_db_path=_path_from(
            source,
            claude_worker.candles.CANDLES_DB_ENV,
            claude_worker.candles.DEFAULT_DB_PATH,
        ),
        regime_dir=claude_worker.regime.regime_dir_for(state_db),
        metrics_url=claude_worker.regime.metrics_url(source),
    )


def write_atomic(path: pathlib.Path, text: str) -> pathlib.Path:
    """Write ``text`` to ``path`` via tmp + ``os.replace`` (spec §4.4).

    Every file output of this lane goes through here: a reader (the
    dashboard, the operator, a later cycle) never sees a half-written file,
    and a crash mid-write leaves the previous version intact.
    """
    path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
    tmp = path.with_name(path.name + ".tmp")
    tmp.write_text(text, encoding="utf-8")
    os.replace(tmp, path)
    return path
