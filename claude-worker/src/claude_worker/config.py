"""Boot-time configuration for the 8f worker (design §5, §10).

Two layers, loaded once at startup, immutable thereafter:

- ``BaseConfig`` — every mode (operator verbs + serve). NEVER contains the
  Anthropic API key: any verb must succeed with ``ANTHROPIC_API_KEY`` unset.
- ``ServeConfig`` — ``claude-worker serve`` only; adds ``ANTHROPIC_API_KEY``.
  The daemon (``llm.py`` seam, constructed in ``daemon.py``) is the only
  consumer (design §5.2).

Secrets come from the project-root ``.env`` (chmod 600). No keychain, no
KMS. Matches the Rust side (``crates/core-config``). The HMAC key and the
API key are excluded from ``repr`` — they must never reach logs.

Convention: full ``import x`` only. No ``from x import y``.
"""

import collections.abc
import dataclasses
import os
import pathlib

# Model identifiers. Kept in one place so upgrades are a single-line edit.
# See CLAUDE.md §"Preferred Claude models for tasks in this repo".
MODEL_BULK: str = "claude-haiku-4-5"  # topic tagging, cheap labels
MODEL_REASONING: str = "claude-sonnet-4-6"  # news labeling, rule parsing
MODEL_STRATEGIST: str = "claude-fable-5"  # ruleset proposals (serve only)

_HMAC_KEY_HEX_LEN: int = 64  # 64 hex chars -> 32-byte key (design §4.1)
_HMAC_KEY_BYTES: int = 32


@dataclasses.dataclass(frozen=True, slots=True)
class BaseConfig:
    """Env-derived config shared by ALL modes. No API key here, by design."""

    ai_ingress_sock: pathlib.Path
    ai_ingress_hmac_key: bytes = dataclasses.field(repr=False)  # never logged
    ai_ruleset_dir: pathlib.Path
    replay_dir: pathlib.Path
    db_path: pathlib.Path
    features_dir: pathlib.Path
    market_map_path: pathlib.Path
    rss_feeds: tuple[str, ...]

    def assert_complete(self) -> None:
        """Fail fast on structurally invalid config (design: fail-fast boot)."""
        if len(self.ai_ingress_hmac_key) != _HMAC_KEY_BYTES:
            raise ValueError("AI_INGRESS_HMAC_KEY must decode to exactly 32 bytes")
        if not self.ai_ingress_sock.is_absolute():
            raise ValueError(f"AI_INGRESS_SOCK must be absolute: {self.ai_ingress_sock}")
        if not self.ai_ruleset_dir.is_absolute():
            raise ValueError(f"AI_RULESET_DIR must be absolute: {self.ai_ruleset_dir}")
        if not self.replay_dir.is_absolute():
            raise ValueError(f"CLAUDE_WORKER_REPLAY_DIR must be absolute: {self.replay_dir}")
        if not self.db_path.is_absolute():
            raise ValueError(f"CLAUDE_WORKER_DB must be absolute: {self.db_path}")
        if not self.features_dir.is_absolute():
            raise ValueError(f"CLAUDE_WORKER_FEATURES_DIR must be absolute: {self.features_dir}")
        if not self.market_map_path.is_absolute():
            raise ValueError(f"CLAUDE_WORKER_MARKET_MAP must be absolute: {self.market_map_path}")


@dataclasses.dataclass(frozen=True, slots=True)
class ServeConfig(BaseConfig):
    """``serve``-only config: BaseConfig + the Anthropic API key (design §5.2)."""

    anthropic_api_key: str = dataclasses.field(repr=False)  # never logged

    def assert_complete(self) -> None:
        BaseConfig.assert_complete(self)
        if not self.anthropic_api_key:
            raise ValueError("ANTHROPIC_API_KEY is empty — `serve` requires it (.env)")
        if not self.anthropic_api_key.startswith("sk-ant-"):
            raise ValueError("ANTHROPIC_API_KEY does not look like an Anthropic key")


def _parse_hmac_key(raw: str) -> bytes:
    """Decode AI_INGRESS_HMAC_KEY (64 hex chars -> 32 bytes).

    Error messages deliberately never echo the provided value.
    """
    if not raw:
        raise ValueError("AI_INGRESS_HMAC_KEY is empty — set 64 hex chars in .env")
    if len(raw) != _HMAC_KEY_HEX_LEN:
        raise ValueError("AI_INGRESS_HMAC_KEY must be exactly 64 hex chars (32 bytes)")
    try:
        return bytes.fromhex(raw)
    except ValueError as exc:
        raise ValueError("AI_INGRESS_HMAC_KEY is not valid hex") from exc


def _parse_rss_feeds(raw: str) -> tuple[str, ...]:
    """RSS_FEEDS CSV -> tuple of URLs; empty/absent -> empty allowlist."""
    parts = raw.split(",")
    out: list[str] = []
    for i in range(len(parts)):
        item = parts[i].strip()
        if item:
            out.append(item)
    return tuple(out)


def _path_from(source: collections.abc.Mapping[str, str], key: str, default: str) -> pathlib.Path:
    raw = source.get(key, "")
    if not raw:
        raw = default
    return pathlib.Path(raw).expanduser()


def load_base_from_env(env: collections.abc.Mapping[str, str] | None = None) -> BaseConfig:
    """Load and validate BaseConfig from the process environment.

    Tests inject ``env={...}`` dicts to avoid touching the real environment.
    ``ANTHROPIC_API_KEY`` is deliberately never read here — the Base/Serve
    split is the enforcement point for "verbs never need the key".
    """
    source: collections.abc.Mapping[str, str] = os.environ if env is None else env
    home = pathlib.Path.home()

    replay_raw = source.get("CLAUDE_WORKER_REPLAY_DIR", "")
    if not replay_raw:
        raise ValueError(
            "CLAUDE_WORKER_REPLAY_DIR is empty — point it at the engine MULTIVENUE_LOG_DIR"
        )

    cfg = BaseConfig(
        ai_ingress_sock=_path_from(source, "AI_INGRESS_SOCK", str(home / "multivenue/run/ai.sock")),
        ai_ingress_hmac_key=_parse_hmac_key(source.get("AI_INGRESS_HMAC_KEY", "")),
        ai_ruleset_dir=_path_from(
            source, "AI_RULESET_DIR", str(home / "multivenue/artifacts/rulesets")
        ),
        replay_dir=pathlib.Path(replay_raw).expanduser(),
        db_path=_path_from(source, "CLAUDE_WORKER_DB", str(home / "multivenue/worker/state.db")),
        features_dir=_path_from(
            source, "CLAUDE_WORKER_FEATURES_DIR", str(home / "multivenue/worker/features")
        ),
        # S6 operator decision (S5 open question 2): the labeling universe
        # ({market name -> SymbolId}) and the HIP-4 (yes,no) pairs live in ONE
        # operator-editable JSON file. Missing file = empty map (valid
        # degraded mode: triage-only serve, no netting view).
        market_map_path=_path_from(
            source, "CLAUDE_WORKER_MARKET_MAP", str(home / "multivenue/worker/market-map.json")
        ),
        rss_feeds=_parse_rss_feeds(source.get("RSS_FEEDS", "")),
    )
    cfg.assert_complete()
    return cfg


def load_serve_from_env(env: collections.abc.Mapping[str, str] | None = None) -> ServeConfig:
    """Load and validate ServeConfig (BaseConfig + ANTHROPIC_API_KEY)."""
    source: collections.abc.Mapping[str, str] = os.environ if env is None else env
    base = load_base_from_env(source)

    cfg = ServeConfig(
        ai_ingress_sock=base.ai_ingress_sock,
        ai_ingress_hmac_key=base.ai_ingress_hmac_key,
        ai_ruleset_dir=base.ai_ruleset_dir,
        replay_dir=base.replay_dir,
        db_path=base.db_path,
        features_dir=base.features_dir,
        market_map_path=base.market_map_path,
        rss_feeds=base.rss_feeds,
        anthropic_api_key=source.get("ANTHROPIC_API_KEY", ""),
    )
    cfg.assert_complete()
    return cfg
