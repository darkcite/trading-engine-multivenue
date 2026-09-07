# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""K1.3 - the per-path forecaster: normalisation, path ordering, the
rolling context, and the distribution helpers.

The forecaster is driven here through a FAKE ``model.kronos`` whose
tokenizer decodes to a value that identifies its own batch row. That is
what makes the two silent-corruption bugs testable: a wrong inverse
z-score (every price plausible, all of them wrong) and a wrong
``[n*s]`` reshape (path 3 of series 0 reported as a path of series 1).
Neither would raise anything in production; both change every number.

Torch is required (it is the optional ``kronos`` group), so the whole
module skips when it is absent.

Convention: full ``import x`` only. No ``from x import y``.
"""

import datetime
import hashlib
import pathlib
import sys
import typing

import pytest

import claude_worker.kronos.install
import tests.test_kronos_upstream

# numpy and torch are the optional `kronos` group: skip the module rather
# than fail collection on a base worker install.
numpy = pytest.importorskip("numpy")
torch = pytest.importorskip("torch")
infer = pytest.importorskip("claude_worker.kronos.infer")

MAX_CONTEXT: int = 16
VOCAB: int = 8

FAKE_KRONOS: str = '''
"""Fake upstream with torch tensors, shaped like the real seams."""

import torch


class _Module:
    def __init__(self, where):
        self.where = where
        self.training = True
        self.calls = 0

    @classmethod
    def from_pretrained(cls, path):
        return cls(path)

    def to(self, device):
        self.device = device
        return self

    def eval(self):
        self.training = False
        return self


class KronosTokenizer(_Module):
    def encode(self, x, half=False):
        batch, length = x.shape[0], x.shape[1]
        zeros = torch.zeros((batch, length), dtype=torch.long, device=x.device)
        return [zeros, zeros.clone()]

    def decode(self, tokens, half=False):
        batch, length = tokens[0].shape[0], tokens[0].shape[1]
        row = torch.arange(batch, dtype=torch.float32, device=tokens[0].device)
        return row.reshape(batch, 1, 1).expand(batch, length, {N_COLS_PLACEHOLDER})


class Kronos(_Module):
    def decode_s1(self, pre, post, stamp):
        self.calls += 1
        batch, window = pre.shape[0], pre.shape[1]
        logits = torch.zeros((batch, window, {VOCAB_PLACEHOLDER}), device=pre.device)
        logits[:, :, 1] = 1.0
        return logits, logits

    def decode_s2(self, context, sample):
        batch, window = context.shape[0], context.shape[1]
        logits = torch.zeros((batch, window, {VOCAB_PLACEHOLDER}), device=context.device)
        logits[:, :, 2] = 1.0
        return logits


def sample_from_logits(logits, temperature=1.0, top_k=None, top_p=None, sample_logits=True):
    return torch.argmax(logits, dim=-1, keepdim=True)
'''.replace("{N_COLS_PLACEHOLDER}", str(infer.N_COLS)).replace(
    "{VOCAB_PLACEHOLDER}", str(VOCAB)
)


def sha(payload: bytes) -> str:
    return hashlib.sha256(payload).hexdigest()


@pytest.fixture(autouse=True)
def clean_import_state() -> typing.Iterator[None]:
    before = list(sys.path)
    yield
    for name in [key for key in sys.modules if key == "model" or key.startswith("model.")]:
        del sys.modules[name]
    sys.path[:] = before


@pytest.fixture
def forecaster_factory(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> typing.Callable[..., typing.Any]:
    def build(*, lookback: int, h_max: int, s_max: int, n_series_max: int = 2) -> typing.Any:
        monkeypatch.setenv(claude_worker.kronos.install.VENDOR_ENV, str(tmp_path / "vendor"))
        monkeypatch.setenv(claude_worker.kronos.install.ARTIFACTS_ENV, str(tmp_path / "artifacts"))
        text = tests.test_kronos_upstream.lock_text(FAKE_KRONOS).replace(
            "max_context = 16", f"max_context = {MAX_CONTEXT}"
        )
        lock_file = tmp_path / "kronos.lock"
        lock_file.write_text(text, encoding="utf-8")
        lock = claude_worker.kronos.install.load_lock(lock_file)
        tree = claude_worker.kronos.install.vendor_dir(lock)
        (tree / "model").mkdir(parents=True, exist_ok=True)
        (tree / "model" / "__init__.py").write_text(
            tests.test_kronos_upstream.INIT_PY, encoding="utf-8"
        )
        (tree / "model" / "kronos.py").write_text(FAKE_KRONOS, encoding="utf-8")
        (tree / "LICENSE").write_text(tests.test_kronos_upstream.LICENSE_TXT, encoding="utf-8")
        model_dir = claude_worker.kronos.install.weights_dir("Fake-model")
        model_dir.mkdir(parents=True, exist_ok=True)
        (model_dir / "model.safetensors").write_bytes(tests.test_kronos_upstream.BLOB)
        tok_dir = claude_worker.kronos.install.weights_dir("Fake-tokenizer")
        tok_dir.mkdir(parents=True, exist_ok=True)
        (tok_dir / "config.json").write_bytes(tests.test_kronos_upstream.CFG)
        return infer.Forecaster(
            "Fake-model",
            "cpu",
            n_series_max=n_series_max,
            lookback=lookback,
            h_max=h_max,
            s_max=s_max,
            lock=lock,
        )

    return build


def series(n: int, length: int, base: float) -> numpy.ndarray:
    """n distinct, non-degenerate OHLCV windows."""
    out = numpy.empty((n, length, infer.N_COLS), dtype=numpy.float32)
    for i in range(n):
        ramp = numpy.linspace(base * (i + 1), base * (i + 1) * 1.05, length, dtype=numpy.float32)
        for col in range(infer.N_COLS):
            out[i, :, col] = ramp * (1.0 + 0.01 * col)
    return out


# ---- stamps --------------------------------------------------------------


def test_stamps_match_the_calendar_pandas_would_give() -> None:
    epochs = numpy.array(
        [
            0,  # 1970-01-01 Thursday 00:00
            1_788_584_642_000,  # a 2026 timestamp
            1_772_323_200_000,  # 2026-02-29 would not exist; a leap-year probe
        ],
        dtype=numpy.int64,
    )
    out = numpy.zeros((3, 5), dtype=numpy.float32)
    infer.stamps_from_epoch_ms(epochs, out)
    for row, epoch in enumerate(epochs):
        when = datetime.datetime.fromtimestamp(int(epoch) / 1000.0, datetime.UTC)
        assert out[row, 0] == when.minute
        assert out[row, 1] == when.hour
        assert out[row, 2] == when.weekday()
        assert out[row, 3] == when.day
        assert out[row, 4] == when.month


def test_stamps_refuse_a_mis_shaped_destination() -> None:
    with pytest.raises(ValueError):
        infer.stamps_from_epoch_ms(
            numpy.zeros(4, dtype=numpy.int64), numpy.zeros((3, 5), dtype=numpy.float32)
        )


# ---- the loop ------------------------------------------------------------


def test_denormalisation_and_path_ordering_are_upstreams(
    forecaster_factory: typing.Callable[..., typing.Any]
) -> None:
    """The fake tokenizer decodes row ``b`` of the batch to the constant
    ``b``. Upstream's batching puts the ``s`` paths of a series ADJACENT
    (``unsqueeze(1).repeat(...).reshape(-1)``), so row ``b = i*s + j`` is
    path ``j`` of series ``i`` — and the inverse z-score must map it to
    ``b * (std + eps) + mean`` of THAT series' window."""
    lookback, h, s, n = 8, 3, 4, 2
    engine = forecaster_factory(lookback=lookback, h_max=h, s_max=s)
    ohlcv = series(n, lookback, 100.0)
    x_stamp = numpy.zeros((n, lookback, infer.N_STAMP), dtype=numpy.float32)
    y_stamp = numpy.zeros((n, h, infer.N_STAMP), dtype=numpy.float32)
    out = numpy.zeros((n, s, h, infer.N_COLS), dtype=numpy.float64)
    engine.forecast(ohlcv, x_stamp, y_stamp, h=h, s=s, t=1.0, top_p=0.9, out=out)
    mean = ohlcv.mean(axis=1)
    std = ohlcv.std(axis=1)
    for i in range(n):
        for j in range(s):
            expected = (i * s + j) * (std[i] + infer.STD_EPS) + mean[i]
            for step in range(h):
                numpy.testing.assert_allclose(out[i, j, step], expected, rtol=1e-5)


def test_every_horizon_step_runs_one_decode(
    forecaster_factory: typing.Callable[..., typing.Any]
) -> None:
    lookback, h, s = 8, 5, 2
    engine = forecaster_factory(lookback=lookback, h_max=h, s_max=s, n_series_max=1)
    before = engine.model.calls
    out = numpy.zeros((1, s, h, infer.N_COLS), dtype=numpy.float64)
    engine.forecast(
        series(1, lookback, 50.0),
        numpy.zeros((1, lookback, infer.N_STAMP), dtype=numpy.float32),
        numpy.zeros((1, h, infer.N_STAMP), dtype=numpy.float32),
        h=h,
        s=s,
        t=1.0,
        top_p=0.9,
        out=out,
    )
    assert engine.model.calls - before == h


def test_the_context_rolls_past_max_context(
    forecaster_factory: typing.Callable[..., typing.Any]
) -> None:
    """lookback + h > max_context takes upstream's `torch.roll` branch;
    if it were skipped the tail tokens would be written off the end."""
    lookback, h, s = MAX_CONTEXT - 2, 5, 2
    engine = forecaster_factory(lookback=lookback, h_max=h, s_max=s, n_series_max=1)
    out = numpy.zeros((1, s, h, infer.N_COLS), dtype=numpy.float64)
    engine.forecast(
        series(1, lookback, 10.0),
        numpy.zeros((1, lookback, infer.N_STAMP), dtype=numpy.float32),
        numpy.zeros((1, h, infer.N_STAMP), dtype=numpy.float32),
        h=h,
        s=s,
        t=1.0,
        top_p=0.9,
        out=out,
    )
    assert numpy.isfinite(out).all()


def test_warmups_ran_at_construction(
    forecaster_factory: typing.Callable[..., typing.Any]
) -> None:
    engine = forecaster_factory(lookback=8, h_max=2, s_max=2, n_series_max=1)
    assert engine.model.calls == infer.WARMUPS  # h=1 per warm-up


@pytest.mark.parametrize("bad", ["n", "h", "s", "ohlcv", "x_stamp", "y_stamp", "out"])
def test_shapes_are_refused_not_broadcast(
    forecaster_factory: typing.Callable[..., typing.Any], bad: str
) -> None:
    lookback, h, s, n = 8, 2, 2, 1
    engine = forecaster_factory(lookback=lookback, h_max=h, s_max=s, n_series_max=n)
    kwargs: dict[str, typing.Any] = {
        "ohlcv": series(n, lookback, 20.0),
        "x_stamp": numpy.zeros((n, lookback, infer.N_STAMP), dtype=numpy.float32),
        "y_stamp": numpy.zeros((n, h, infer.N_STAMP), dtype=numpy.float32),
        "h": h,
        "s": s,
        "t": 1.0,
        "top_p": 0.9,
        "out": numpy.zeros((n, s, h, infer.N_COLS), dtype=numpy.float64),
    }
    if bad == "n":
        kwargs["ohlcv"] = series(n + 1, lookback, 20.0)
    elif bad == "h":
        kwargs["h"] = h + 1
    elif bad == "s":
        kwargs["s"] = s + 1
    elif bad == "ohlcv":
        kwargs["ohlcv"] = series(n, lookback - 1, 20.0)
    elif bad == "x_stamp":
        kwargs["x_stamp"] = numpy.zeros((n, lookback, 4), dtype=numpy.float32)
    elif bad == "y_stamp":
        kwargs["y_stamp"] = numpy.zeros((n, h + 1, infer.N_STAMP), dtype=numpy.float32)
    else:
        kwargs["out"] = numpy.zeros((n, s, h - 1, infer.N_COLS), dtype=numpy.float64)
    with pytest.raises(ValueError):
        engine.forecast(**kwargs)


def test_lookback_beyond_max_context_is_refused(
    forecaster_factory: typing.Callable[..., typing.Any]
) -> None:
    with pytest.raises(ValueError):
        forecaster_factory(lookback=MAX_CONTEXT + 1, h_max=2, s_max=2)


# ---- distribution helpers ------------------------------------------------


def make_paths(terminal: numpy.ndarray) -> numpy.ndarray:
    """``[n, s, h, 6]`` whose terminal closes are `terminal[n, s]`."""
    n, s = terminal.shape
    out = numpy.zeros((n, s, 2, infer.N_COLS), dtype=numpy.float64)
    out[:, :, 1, infer.COL_CLOSE] = terminal
    out[:, :, 0, infer.COL_CLOSE] = terminal
    out[:, :, :, infer.COL_HIGH] = terminal[:, :, None] * 1.10
    out[:, :, :, infer.COL_LOW] = terminal[:, :, None] * 0.90
    return out


def test_terminal_quantiles_are_the_empirical_cdf() -> None:
    terminal = numpy.array([[1.0, 2.0, 3.0, 4.0]], dtype=numpy.float64)
    paths = make_paths(terminal)
    levels = numpy.array([0.25, 0.5, 0.75])
    dst = numpy.zeros((1, 3), dtype=numpy.float64)
    infer.terminal_quantiles(paths, 2, levels, dst)
    numpy.testing.assert_allclose(dst[0], numpy.quantile(terminal[0], levels))
    mean = numpy.zeros(1, dtype=numpy.float64)
    infer.terminal_mean(paths, 2, mean)
    assert mean[0] == pytest.approx(2.5)


def test_range_quantiles_take_min_of_low_and_max_of_high() -> None:
    terminal = numpy.array([[1.0, 2.0, 3.0, 4.0]], dtype=numpy.float64)
    paths = make_paths(terminal)
    dst = numpy.zeros((1, 2), dtype=numpy.float64)
    infer.range_quantiles(paths, 2, 0.25, dst)
    lows = terminal[0] * 0.90
    highs = terminal[0] * 1.10
    assert dst[0, 0] == pytest.approx(float(numpy.quantile(lows, 0.25)))
    assert dst[0, 1] == pytest.approx(float(numpy.quantile(highs, 0.75)))


def test_sigma_bps_is_scale_free() -> None:
    """sd(log P_T) does not move when every path is multiplied by a
    constant - which is what makes the anchor argument unnecessary."""
    terminal = numpy.array([[100.0, 101.0, 99.0, 100.5]], dtype=numpy.float64)
    dst = numpy.zeros(1, dtype=numpy.float64)
    infer.sigma_bps(make_paths(terminal), 2, dst)
    scaled = numpy.zeros(1, dtype=numpy.float64)
    infer.sigma_bps(make_paths(terminal * 7.0), 2, scaled)
    assert dst[0] == pytest.approx(scaled[0])
    expected = numpy.std(numpy.log(terminal[0])) * infer.BPS
    assert dst[0] == pytest.approx(expected)
