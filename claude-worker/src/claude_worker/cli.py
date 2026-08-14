"""CLI entry point — ``uv run claude-worker ...``.

Sub-commands:
  tag-topics    run Haiku bulk topic tagger over an NDJSON file
  parse-rules   run Sonnet rule parser over a notes file

Convention: full ``import x`` only. No ``from x import y``.
"""

import json
import pathlib
import sys
import typing

import typer

import claude_worker.config
import claude_worker.rule_parser
import claude_worker.topic_tagger


app: typer.Typer = typer.Typer(
    add_completion=False,
    no_args_is_help=True,
    help="Offline Claude strategy-research worker for the Polymarket engine.",
)


def _load_ndjson_items(path: pathlib.Path) -> list[tuple[str, str]]:
    """Load ``(id, text)`` pairs from an NDJSON file.

    Each line must be an object with ``id`` and ``text`` keys.
    """
    out: list[tuple[str, str]] = []
    with path.open("r", encoding="utf-8") as fh:
        line_no = 0
        while True:
            line_no += 1
            line = fh.readline()
            if not line:
                break
            stripped = line.strip()
            if not stripped:
                continue
            obj = json.loads(stripped)
            if not isinstance(obj, dict):
                raise ValueError(f"{path}:{line_no}: line is not a JSON object")
            record_id = str(obj.get("id", ""))
            text = str(obj.get("text", ""))
            if not record_id or not text:
                raise ValueError(f"{path}:{line_no}: missing id or text")
            out.append((record_id, text))
    return out


@app.command("tag-topics")
def tag_topics_cmd(
    input_path: pathlib.Path = typer.Option(..., "--input", "-i", exists=True, readable=True),
    output_path: pathlib.Path = typer.Option(..., "--output", "-o"),
) -> None:
    """Tag topics for every record in ``input_path`` (NDJSON)."""
    cfg = claude_worker.config.load_from_env()
    items = _load_ndjson_items(input_path)
    tags = claude_worker.topic_tagger.tag_batch(cfg, items)
    claude_worker.topic_tagger.write_artifact(tags, output_path)
    typer.echo(f"wrote {len(tags)} tags -> {output_path}")


@app.command("parse-rules")
def parse_rules_cmd(
    input_path: pathlib.Path = typer.Option(..., "--input", "-i", exists=True, readable=True),
    output_path: pathlib.Path = typer.Option(..., "--output", "-o"),
) -> None:
    """Parse each line of ``input_path`` as a research note → ``StrategyRule``."""
    cfg = claude_worker.config.load_from_env()

    rules: list[claude_worker.rule_parser.StrategyRule] = []
    with input_path.open("r", encoding="utf-8") as fh:
        while True:
            line = fh.readline()
            if not line:
                break
            stripped = line.strip()
            if not stripped:
                continue
            rules.append(claude_worker.rule_parser.parse_note(cfg, stripped))

    claude_worker.rule_parser.write_artifact(rules, output_path)
    typer.echo(f"wrote {len(rules)} rules -> {output_path}")


def main(argv: typing.Sequence[str] | None = None) -> None:
    """Entry point exposed as the ``claude-worker`` console script."""
    if argv is None:
        app()
    else:
        # Typer uses click under the hood — passing args explicitly helps tests.
        app(args=list(argv), standalone_mode=False)


if __name__ == "__main__":
    main(sys.argv[1:])
