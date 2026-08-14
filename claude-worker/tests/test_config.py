"""Tests for claude_worker.config.

Convention: full ``import x`` only. No ``from x import y``.
"""

import pathlib

import pytest

import claude_worker.config


def test_load_from_env_happy_path() -> None:
    cfg = claude_worker.config.load_from_env(
        env={  # type: ignore[arg-type]
            "ANTHROPIC_API_KEY": "sk-ant-abc123",
            "CLAUDE_WORKER_ARTIFACTS_DIR": "/tmp/arts",
            "CLAUDE_WORKER_LOG_DIR": "/tmp/logs",
        }
    )
    assert cfg.anthropic_api_key == "sk-ant-abc123"
    assert cfg.artifacts_dir == pathlib.Path("/tmp/arts")
    assert cfg.log_dir == pathlib.Path("/tmp/logs")
    assert cfg.default_model == claude_worker.config.MODEL_BULK


def test_load_from_env_missing_key_fails_fast() -> None:
    with pytest.raises(ValueError, match="empty"):
        claude_worker.config.load_from_env(env={})  # type: ignore[arg-type]


def test_load_from_env_malformed_key_fails_fast() -> None:
    with pytest.raises(ValueError, match="does not look like"):
        claude_worker.config.load_from_env(
            env={"ANTHROPIC_API_KEY": "definitely-not-an-anthropic-key"}  # type: ignore[arg-type]
        )


def test_model_ids_are_stable() -> None:
    """Model IDs are load-bearing — a typo breaks the whole pipeline.

    Keep this test brittle on purpose: if you upgrade a model, update here too.
    """
    assert claude_worker.config.MODEL_BULK == "claude-haiku-4-5"
    assert claude_worker.config.MODEL_REASONING == "claude-sonnet-4-6"
    assert claude_worker.config.MODEL_HARD == "claude-opus-4-6"
