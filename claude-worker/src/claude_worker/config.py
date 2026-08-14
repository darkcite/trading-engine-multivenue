"""Boot-time configuration for the offline worker.

Secrets come from the project-root ``.env`` file. No keychain, no KMS, no
Secrets Manager. This matches the Rust side (see ``crates/core-config``).

Convention: full ``import x`` only. No ``from x import y``.
"""

import dataclasses
import os
import pathlib


# Model identifiers. Kept in one place so upgrades are a single-line edit.
# See CLAUDE.md §"Preferred Claude models for tasks in this repo".
MODEL_BULK: str = "claude-haiku-4-5"            # topic tagging, cheap labels
MODEL_REASONING: str = "claude-sonnet-4-6"      # rule parsing, news labeling
MODEL_HARD: str = "claude-opus-4-6"             # backtest review, architecture


@dataclasses.dataclass(frozen=True, slots=True)
class WorkerConfig:
    """Immutable configuration loaded once at startup."""

    anthropic_api_key: str
    artifacts_dir: pathlib.Path
    log_dir: pathlib.Path
    default_model: str = MODEL_BULK

    def assert_complete(self) -> None:
        """Fail fast if required fields are missing or pointing at bad paths."""
        if not self.anthropic_api_key:
            raise ValueError("ANTHROPIC_API_KEY is empty — set it in .env")
        if not self.anthropic_api_key.startswith("sk-ant-"):
            raise ValueError("ANTHROPIC_API_KEY does not look like an Anthropic key")
        if not self.artifacts_dir.is_absolute():
            raise ValueError(f"artifacts_dir must be absolute: {self.artifacts_dir}")
        if not self.log_dir.is_absolute():
            raise ValueError(f"log_dir must be absolute: {self.log_dir}")


def load_from_env(env: os._Environ[str] | None = None) -> WorkerConfig:
    """Load config from the process environment.

    Tests pass ``env={"ANTHROPIC_API_KEY": "sk-ant-test", ...}`` to avoid
    touching the real environment.
    """
    source: os._Environ[str] | dict[str, str]
    source = env if env is not None else os.environ

    api_key = source.get("ANTHROPIC_API_KEY", "")
    artifacts_raw = source.get("CLAUDE_WORKER_ARTIFACTS_DIR", "")
    log_raw = source.get("CLAUDE_WORKER_LOG_DIR", "")

    if not artifacts_raw:
        artifacts_raw = str(pathlib.Path.home() / "polymarket" / "artifacts")
    if not log_raw:
        log_raw = str(pathlib.Path.home() / "polymarket" / "logs" / "worker")

    cfg = WorkerConfig(
        anthropic_api_key=api_key,
        artifacts_dir=pathlib.Path(artifacts_raw),
        log_dir=pathlib.Path(log_raw),
    )
    cfg.assert_complete()
    return cfg
