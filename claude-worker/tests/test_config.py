"""Tests for claude_worker.config — the Base/Serve split (design §5.2, §10).

Convention: full ``import x`` only. No ``from x import y``.
"""

import pathlib

import pytest

import claude_worker.config

_HMAC_HEX: str = "ab" * 32  # 64 hex chars -> b"\xab" * 32
_BASE_ENV: dict[str, str] = {
    "AI_INGRESS_SOCK": "/tmp/stage2-test/ai.sock",
    "AI_INGRESS_HMAC_KEY": _HMAC_HEX,
    "AI_RULESET_DIR": "/tmp/stage2-test/rulesets",
    "CLAUDE_WORKER_REPLAY_DIR": "/tmp/stage2-test/logs",
    "CLAUDE_WORKER_DB": "/tmp/stage2-test/state.db",
    "CLAUDE_WORKER_FEATURES_DIR": "/tmp/stage2-test/features",
    "RSS_FEEDS": "https://a.example/feed.xml, https://b.example/rss ,",
}


# ---------------------------------------------------------------- BaseConfig


def test_load_base_happy_path() -> None:
    cfg = claude_worker.config.load_base_from_env(env=_BASE_ENV)
    assert cfg.ai_ingress_sock == pathlib.Path("/tmp/stage2-test/ai.sock")
    assert cfg.ai_ingress_hmac_key == b"\xab" * 32
    assert cfg.ai_ruleset_dir == pathlib.Path("/tmp/stage2-test/rulesets")
    assert cfg.replay_dir == pathlib.Path("/tmp/stage2-test/logs")
    assert cfg.db_path == pathlib.Path("/tmp/stage2-test/state.db")
    assert cfg.features_dir == pathlib.Path("/tmp/stage2-test/features")
    assert cfg.rss_feeds == ("https://a.example/feed.xml", "https://b.example/rss")


def test_load_base_defaults_applied() -> None:
    env = {
        "AI_INGRESS_HMAC_KEY": _HMAC_HEX,
        "CLAUDE_WORKER_REPLAY_DIR": "/tmp/stage2-test/logs",
    }
    cfg = claude_worker.config.load_base_from_env(env=env)
    home = pathlib.Path.home()
    assert cfg.ai_ingress_sock == home / "multivenue/run/ai.sock"
    assert cfg.ai_ruleset_dir == home / "multivenue/artifacts/rulesets"
    assert cfg.db_path == home / "multivenue/worker/state.db"
    assert cfg.features_dir == home / "multivenue/worker/features"
    assert cfg.rss_feeds == ()


def test_load_base_expands_tilde() -> None:
    env = dict(_BASE_ENV)
    env["AI_INGRESS_SOCK"] = "~/stage2-test/ai.sock"
    cfg = claude_worker.config.load_base_from_env(env=env)
    assert cfg.ai_ingress_sock == pathlib.Path.home() / "stage2-test/ai.sock"
    assert cfg.ai_ingress_sock.is_absolute()


def test_load_base_ignores_anthropic_key() -> None:
    """The split invariant: BaseConfig never reads ANTHROPIC_API_KEY."""
    env = dict(_BASE_ENV)
    env["ANTHROPIC_API_KEY"] = "sk-ant-should-be-invisible"
    cfg = claude_worker.config.load_base_from_env(env=env)
    assert not hasattr(cfg, "anthropic_api_key")


def test_load_base_missing_replay_dir_fails_fast() -> None:
    env = dict(_BASE_ENV)
    del env["CLAUDE_WORKER_REPLAY_DIR"]
    with pytest.raises(ValueError, match="CLAUDE_WORKER_REPLAY_DIR is empty"):
        claude_worker.config.load_base_from_env(env=env)


def test_load_base_missing_hmac_key_fails_fast() -> None:
    env = dict(_BASE_ENV)
    del env["AI_INGRESS_HMAC_KEY"]
    with pytest.raises(ValueError, match="AI_INGRESS_HMAC_KEY is empty"):
        claude_worker.config.load_base_from_env(env=env)


def test_load_base_short_hmac_key_fails_fast() -> None:
    env = dict(_BASE_ENV)
    env["AI_INGRESS_HMAC_KEY"] = "abcd"
    with pytest.raises(ValueError, match="exactly 64 hex chars"):
        claude_worker.config.load_base_from_env(env=env)


def test_load_base_non_hex_hmac_key_fails_fast() -> None:
    env = dict(_BASE_ENV)
    env["AI_INGRESS_HMAC_KEY"] = "zz" * 32  # right length, not hex
    with pytest.raises(ValueError, match="not valid hex"):
        claude_worker.config.load_base_from_env(env=env)


def test_load_base_relative_path_fails_fast() -> None:
    env = dict(_BASE_ENV)
    env["AI_INGRESS_SOCK"] = "relative/ai.sock"
    with pytest.raises(ValueError, match="AI_INGRESS_SOCK must be absolute"):
        claude_worker.config.load_base_from_env(env=env)


def test_base_repr_never_leaks_hmac_key() -> None:
    """The HMAC key is repr=False — it must never reach logs via repr/str."""
    cfg = claude_worker.config.load_base_from_env(env=_BASE_ENV)
    shown = repr(cfg)
    assert _HMAC_HEX not in shown
    assert "ai_ingress_hmac_key" not in shown
    assert repr(b"\xab" * 32) not in shown


# --------------------------------------------------------------- ServeConfig


def test_load_serve_happy_path() -> None:
    env = dict(_BASE_ENV)
    env["ANTHROPIC_API_KEY"] = "sk-ant-test-key-123"
    cfg = claude_worker.config.load_serve_from_env(env=env)
    assert cfg.anthropic_api_key == "sk-ant-test-key-123"
    assert cfg.ai_ingress_hmac_key == b"\xab" * 32  # base fields carried


def test_load_serve_missing_api_key_fails_fast() -> None:
    with pytest.raises(ValueError, match="ANTHROPIC_API_KEY is empty"):
        claude_worker.config.load_serve_from_env(env=_BASE_ENV)


def test_load_serve_malformed_api_key_fails_fast() -> None:
    env = dict(_BASE_ENV)
    env["ANTHROPIC_API_KEY"] = "definitely-not-an-anthropic-key"
    with pytest.raises(ValueError, match="does not look like"):
        claude_worker.config.load_serve_from_env(env=env)


def test_serve_repr_never_leaks_secrets() -> None:
    env = dict(_BASE_ENV)
    env["ANTHROPIC_API_KEY"] = "sk-ant-test-key-123"
    cfg = claude_worker.config.load_serve_from_env(env=env)
    shown = repr(cfg)
    assert "sk-ant-test-key-123" not in shown
    assert _HMAC_HEX not in shown


def test_serve_is_a_base_config() -> None:
    """Verbs typed against BaseConfig accept a ServeConfig (one code path)."""
    env = dict(_BASE_ENV)
    env["ANTHROPIC_API_KEY"] = "sk-ant-test-key-123"
    cfg = claude_worker.config.load_serve_from_env(env=env)
    assert isinstance(cfg, claude_worker.config.BaseConfig)


# ------------------------------------------------------------- model consts


def test_model_ids_are_stable() -> None:
    """Model IDs are load-bearing — a typo breaks the whole pipeline.

    Brittle on purpose: upgrading a model means updating here too.
    """
    assert claude_worker.config.MODEL_BULK == "claude-haiku-4-5"
    assert claude_worker.config.MODEL_REASONING == "claude-sonnet-4-6"
    assert claude_worker.config.MODEL_STRATEGIST == "claude-fable-5"
