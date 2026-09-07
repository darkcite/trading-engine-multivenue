# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""K1.3 - the per-path forecaster (plan §3a.3, §14.2).

Upstream's ``auto_regressive_inference`` ends with
``preds = np.mean(preds, axis=1)``: it draws ``sample_count`` stochastic
paths and then throws the distribution away. Every lane in this plan
consumes the *distribution* - the terminal CDF (lane P), path-min/max
quantiles (lane M), forward sigma (lanes R and V) - so this module runs
the same loop over upstream's own public pieces and **keeps the paths**.

Everything else is upstream's law, deliberately unchanged so the KK
parity gate can be exact:

* per-window z-score per column over the lookback, ``clip = +/-5``, and
  the inverse transform on the way out (``KronosPredictor.predict``);
* ``tokenizer.encode(x, half=True)`` once, then per step
  ``decode_s1 -> sample_from_logits -> decode_s2 -> sample_from_logits``,
  then ``tokenizer.decode(..., half=True)`` once;
* the rolling ``max_context`` token buffers and the stamp window;
* ``top_k`` and ``top_p`` are mutually exclusive upstream (its filter
  short-circuits), so this API exposes ``top_p`` only and always passes
  ``top_k = 0`` - the ``KronosPredictor`` default.

Deltas from upstream, all deliberate:

* paths are returned, not averaged;
* time stamps are built with numpy (:func:`stamps_from_epoch_ms`), not
  ``pandas`` via ``calc_time_stamps`` - the sidecar has no DataFrame in
  the loop;
* buffers are preallocated at construction and sliced per call. torch's
  own intermediates still allocate - that is unavoidable inside the
  model - but nothing this module owns grows per call;
* modules are in eval mode (:mod:`claude_worker.kronos.weights`).

This is a SIDECAR, not a hot path: it never runs inside the engine, and
its output reaches the engine as 64-byte AI command frames.

Convention: full ``import x`` only. No ``from x import y``.
"""

import gc
import typing

import numpy
import torch

import claude_worker.kronos.install
import claude_worker.kronos.upstream
import claude_worker.kronos.weights

MS_PER_DAY: int = 86_400_000
MS_PER_HOUR: int = 3_600_000
MS_PER_MIN: int = 60_000
# 1970-01-01 was a Thursday; pandas' `.weekday` is Monday-origin.
EPOCH_WEEKDAY_OFFSET: int = 3
CLIP: float = 5.0
STD_EPS: float = 1e-5
N_COLS: int = 6
N_STAMP: int = 5
COL_HIGH: int = 1
COL_LOW: int = 2
COL_CLOSE: int = 3
WARMUPS: int = 3
BPS: float = 10_000.0


def stamps_from_epoch_ms(epoch_ms: numpy.ndarray, out: numpy.ndarray) -> None:
    """``[minute, hour, weekday, day, month]`` into ``out[..., 5]``.

    Byte-for-byte the quantities ``calc_time_stamps`` derives from a
    pandas DatetimeIndex, without pandas: minute/hour/weekday come from
    integer arithmetic on the epoch, day/month from numpy's own calendar
    (``datetime64[M]`` truncation), so leap years and month lengths stay
    the C library's problem rather than ours."""
    if out.shape[:-1] != epoch_ms.shape or out.shape[-1] != N_STAMP:
        raise ValueError(f"stamps: out {out.shape} does not fit epoch_ms {epoch_ms.shape}")
    ms = epoch_ms.astype(numpy.int64)
    days = ms // MS_PER_DAY
    ms_of_day = ms - days * MS_PER_DAY
    as_day = ms.astype("datetime64[ms]").astype("datetime64[D]")
    as_month = as_day.astype("datetime64[M]")
    out[..., 0] = (ms_of_day // MS_PER_MIN) % 60
    out[..., 1] = ms_of_day // MS_PER_HOUR
    out[..., 2] = (days + EPOCH_WEEKDAY_OFFSET) % 7
    out[..., 3] = (as_day - as_month).astype(numpy.int64) + 1
    out[..., 4] = as_month.astype(numpy.int64) % 12 + 1


def terminal_mean(out: numpy.ndarray, h: int, dst: numpy.ndarray) -> None:
    """Mean terminal close per series - the CLOSE wire field's price."""
    dst[:] = out[:, :, h - 1, COL_CLOSE].mean(axis=1)


def terminal_quantiles(
    out: numpy.ndarray, h: int, levels: numpy.ndarray, dst: numpy.ndarray
) -> None:
    """``dst[n, len(levels)]`` = terminal-close quantiles at ``levels``.

    This is the empirical CDF lane P prices a binary against: with the 16
    levels ``(i + 0.5)/16`` it is the wire's TERM_Q ladder."""
    dst[:] = numpy.quantile(out[:, :, h - 1, COL_CLOSE], levels, axis=1).T


def range_quantiles(out: numpy.ndarray, k: int, alpha: float, dst: numpy.ndarray) -> None:
    """``dst[n, 2]`` = (alpha-quantile of the path MINIMUM low over bars
    1..k, (1-alpha)-quantile of the path MAXIMUM high) - lane M's
    bracket."""
    dst[:, 0] = numpy.quantile(out[:, :, :k, COL_LOW].min(axis=2), alpha, axis=1)
    dst[:, 1] = numpy.quantile(out[:, :, :k, COL_HIGH].max(axis=2), 1.0 - alpha, axis=1)


def sigma_bps(out: numpy.ndarray, h: int, dst: numpy.ndarray) -> None:
    """``dst[n]`` = sigma of the terminal LOG return, in bps.

    No anchor argument is needed and none is wanted: the anchor is a
    constant per series, so sd(log P_T / anchor) == sd(log P_T), and
    taking it from the paths alone keeps the estimate independent of
    which bar the caller calls "now"."""
    terminal = out[:, :, h - 1, COL_CLOSE]
    floor = numpy.finfo(numpy.float64).tiny
    dst[:] = numpy.std(numpy.log(numpy.maximum(terminal, floor)), axis=1)
    dst *= BPS


class Forecaster:
    """One loaded checkpoint plus its preallocated working set.

    Construction loads the weights (verifying them against the lock),
    allocates for the worst shape the caller declares, runs
    :data:`WARMUPS` throwaway forecasts (MPS compiles kernels on first
    use - an un-warmed first call is several times its steady-state cost
    and would otherwise land inside a live decision), and freezes the
    GC's view of everything allocated so far."""

    def __init__(  # noqa: PLR0913 - the shape bounds ARE the constructor's contract (§14.2)
        self,
        name: str,
        device: str,
        *,
        n_series_max: int,
        lookback: int,
        h_max: int,
        s_max: int,
        lock: dict[str, typing.Any] | None = None,
    ) -> None:
        if min(n_series_max, lookback, h_max, s_max) < 1:
            raise ValueError("Forecaster: every shape bound must be >= 1")
        self.name = name
        self.device = device
        self.lock = claude_worker.kronos.install.load_lock() if lock is None else lock
        self.tokenizer, self.model, self.max_context = claude_worker.kronos.weights.load(
            name, device, self.lock
        )
        if lookback > self.max_context:
            raise ValueError(
                f"Forecaster: lookback {lookback} exceeds {name}'s max_context {self.max_context}"
            )
        self.n_series_max = n_series_max
        self.lookback = lookback
        self.h_max = h_max
        self.s_max = s_max
        self._kronos = claude_worker.kronos.upstream.activate(self.lock)
        batch = n_series_max * s_max
        # Staging (numpy, host) and the device-side working set.
        self._norm = numpy.empty((n_series_max, lookback, N_COLS), dtype=numpy.float32)
        self._mean = numpy.empty((n_series_max, N_COLS), dtype=numpy.float32)
        self._std = numpy.empty((n_series_max, N_COLS), dtype=numpy.float32)
        self._x = torch.empty((batch, lookback, N_COLS), dtype=torch.float32, device=device)
        self._stamp = torch.empty(
            (batch, lookback + h_max, N_STAMP), dtype=torch.float32, device=device
        )
        self._pre = torch.zeros((batch, self.max_context), dtype=torch.long, device=device)
        self._post = torch.zeros((batch, self.max_context), dtype=torch.long, device=device)
        self._gen_pre = torch.zeros((batch, h_max), dtype=torch.long, device=device)
        self._gen_post = torch.zeros((batch, h_max), dtype=torch.long, device=device)
        self._warm()
        gc.freeze()

    # ---- internals -------------------------------------------------------

    def _warm(self) -> None:
        ramp = numpy.linspace(1.0, 1.1, self.lookback, dtype=numpy.float32).reshape(-1, 1)
        ohlcv = numpy.tile(ramp, (1, N_COLS)).reshape(1, self.lookback, N_COLS)
        x_stamp = numpy.zeros((1, self.lookback, N_STAMP), dtype=numpy.float32)
        y_stamp = numpy.zeros((1, 1, N_STAMP), dtype=numpy.float32)
        out = numpy.empty((1, 1, 1, N_COLS), dtype=numpy.float64)
        for _ in range(WARMUPS):
            self.forecast(ohlcv, x_stamp, y_stamp, h=1, s=1, t=1.0, top_p=0.9, out=out)

    def _sync(self) -> None:
        if self.device.startswith("mps"):
            torch.mps.synchronize()
        elif self.device.startswith("cuda"):
            torch.cuda.synchronize()

    def _normalise(self, ohlcv: numpy.ndarray, n: int) -> tuple[numpy.ndarray, numpy.ndarray]:
        """Per-window z-score, exactly ``KronosPredictor.predict``'s law.
        Returns the (mean, std) views the inverse transform needs."""
        mean = self._mean[:n]
        std = self._std[:n]
        numpy.mean(ohlcv, axis=1, out=mean)
        numpy.std(ohlcv, axis=1, out=std)
        norm = self._norm[:n]
        numpy.subtract(ohlcv, mean[:, None, :], out=norm)
        numpy.divide(norm, (std + STD_EPS)[:, None, :], out=norm)
        numpy.clip(norm, -CLIP, CLIP, out=norm)
        return mean, std

    def _stage(
        self, x_stamp: numpy.ndarray, y_stamp: numpy.ndarray, n: int, s: int, h: int
    ) -> tuple[torch.Tensor, torch.Tensor]:
        """``[n, ...] -> [n*s, ...]`` with the s copies adjacent, matching
        upstream's ``unsqueeze(1).repeat(...).reshape(-1)`` ordering (the
        final reshape back to ``[n, s, ...]`` depends on it)."""
        batch = n * s
        x = self._x[:batch, : self.lookback]
        x.copy_(torch.from_numpy(self._norm[:n]).repeat_interleave(s, dim=0))
        stamp = self._stamp[:batch, : self.lookback + h]
        stamp[:, : self.lookback].copy_(torch.from_numpy(x_stamp[:n]).repeat_interleave(s, dim=0))
        stamp[:, self.lookback :].copy_(torch.from_numpy(y_stamp[:n]).repeat_interleave(s, dim=0))
        return x, stamp

    def _sample(self, logits: torch.Tensor, t: float, top_p: float) -> torch.Tensor:
        return typing.cast(
            torch.Tensor,
            self._kronos.sample_from_logits(
                logits[:, -1, :], temperature=t, top_k=0, top_p=top_p, sample_logits=True
            ),
        )

    def _ar_loop(
        self, x: torch.Tensor, stamp: torch.Tensor, h: int, t: float, top_p: float
    ) -> torch.Tensor:
        """Upstream's autoregressive loop over its public pieces; returns
        the decoded window ``[batch, <= max_context, 6]`` (normalised)."""
        batch = int(x.shape[0])
        token = self.tokenizer.encode(x, half=True)
        pre = self._pre[:batch]
        post = self._post[:batch]
        pre.zero_()
        post.zero_()
        window = min(self.lookback, self.max_context)
        start = max(0, self.lookback - self.max_context)
        pre[:, :window] = token[0][:, start : start + window]
        post[:, :window] = token[1][:, start : start + window]
        gen_pre = self._gen_pre[:batch, :h]
        gen_post = self._gen_post[:batch, :h]
        for step in range(h):
            seq_len = self.lookback + step
            window = min(seq_len, self.max_context)
            current = stamp[:, max(0, seq_len - self.max_context) : seq_len, :].contiguous()
            s1_logits, context = self.model.decode_s1(pre[:, :window], post[:, :window], current)
            sample_pre = self._sample(s1_logits, t, top_p)
            sample_post = self._sample(self.model.decode_s2(context, sample_pre), t, top_p)
            gen_pre[:, step] = sample_pre.squeeze(-1)
            gen_post[:, step] = sample_post.squeeze(-1)
            if seq_len < self.max_context:
                pre[:, seq_len] = sample_pre.squeeze(-1)
                post[:, seq_len] = sample_post.squeeze(-1)
            else:
                pre.copy_(torch.roll(pre, shifts=-1, dims=1))
                post.copy_(torch.roll(post, shifts=-1, dims=1))
                pre[:, -1] = sample_pre.squeeze(-1)
                post[:, -1] = sample_post.squeeze(-1)
        total = self.lookback + h
        tail = max(0, total - self.max_context)
        full_pre = torch.cat([token[0], gen_pre], dim=1)[:, tail:total].contiguous()
        full_post = torch.cat([token[1], gen_post], dim=1)[:, tail:total].contiguous()
        return typing.cast(torch.Tensor, self.tokenizer.decode([full_pre, full_post], half=True))

    # ---- the call --------------------------------------------------------

    def forecast(  # noqa: PLR0913 - the pinned §14.2 signature
        self,
        ohlcv: numpy.ndarray,
        x_stamp: numpy.ndarray,
        y_stamp: numpy.ndarray,
        *,
        h: int,
        s: int,
        t: float,
        top_p: float,
        out: numpy.ndarray,
    ) -> None:
        """``ohlcv[n, L, 6]`` -> ``out[n, s, h, 6]``, denormalised.

        Columns are upstream's: open, high, low, close, volume, amount
        (``amount = volume * mean(o,h,l,c)`` when the venue gives none -
        that substitution belongs to the caller, as it does upstream)."""
        n = int(ohlcv.shape[0])
        self._check(ohlcv, x_stamp, y_stamp, (n, h, s), out)
        mean, std = self._normalise(ohlcv, n)
        x, stamp = self._stage(x_stamp, y_stamp, n, s, h)
        with torch.no_grad():
            decoded = self._ar_loop(x, stamp, h, t, top_p)
            self._sync()
            paths = decoded[:, -h:, :].reshape(n, s, h, N_COLS).to("cpu").numpy()
        numpy.multiply(paths, (std + STD_EPS)[:, None, None, :], out=out[:n, :s, :h])
        numpy.add(out[:n, :s, :h], mean[:, None, None, :], out=out[:n, :s, :h])

    def _check(
        self,
        ohlcv: numpy.ndarray,
        x_stamp: numpy.ndarray,
        y_stamp: numpy.ndarray,
        shape: tuple[int, int, int],
        out: numpy.ndarray,
    ) -> None:
        n, h, s = shape
        if not 1 <= n <= self.n_series_max:
            raise ValueError(f"forecast: n {n} outside 1..{self.n_series_max}")
        if not 1 <= h <= self.h_max:
            raise ValueError(f"forecast: h {h} outside 1..{self.h_max}")
        if not 1 <= s <= self.s_max:
            raise ValueError(f"forecast: s {s} outside 1..{self.s_max}")
        if ohlcv.shape != (n, self.lookback, N_COLS):
            raise ValueError(f"forecast: ohlcv {ohlcv.shape} != {(n, self.lookback, N_COLS)}")
        if x_stamp.shape != (n, self.lookback, N_STAMP):
            raise ValueError(f"forecast: x_stamp {x_stamp.shape}")
        if y_stamp.shape != (n, h, N_STAMP):
            raise ValueError(f"forecast: y_stamp {y_stamp.shape}")
        if out.shape[0] < n or out.shape[1] < s or out.shape[2] < h or out.shape[3] != N_COLS:
            raise ValueError(f"forecast: out {out.shape} too small for {(n, s, h, N_COLS)}")
