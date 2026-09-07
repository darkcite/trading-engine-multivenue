# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""KK - the Kronos golden-parity gate (Kronos lanes plan §3a.4, K1.4).

The adapter in :mod:`claude_worker.kronos` deliberately does NOT
reimplement Kronos: it drives upstream's own public pieces. This gate is
what makes that claim checkable. The fixtures under
``tests/fixtures/kronos/`` were produced on the research test bed by the
C0.3 generator (a `tools_` one-shot, git-excluded by policy - see
``docs/arch/research-tools-exclusion-plan.md``) running the SAME pinned
snapshot and the SAME pinned weights on the same torch build, so any
difference here is the adapter's, not the model's.

Four stages, coarse to fine, each with the tolerance it deserves:

* **token ids** - exact integer equality. Tokens are discrete; "close"
  is meaningless.
* **s1 / s2 logits at the last position** - ``max |delta| == 0.0``,
  literally zero. Same code, same weights, same device is bit-identical
  arithmetic; the fixtures carry a per-case eval-vs-eval control that
  was 0.0 when they were generated, so a non-zero here is a real change
  in what we run, never noise. This is also the assertion that catches
  a module left in TRAINING mode: dropout would perturb the logits
  while leaving everything else plausible.
* **decoded OHLC of a fixed token sequence** - relative 1e-6, since the
  decode path is float arithmetic we do not control the order of.
* **the full per-path loop** - relative 1e-6. This is the stage that
  actually exercises the adapter: stamp construction (numpy here,
  pandas upstream), the rolling ``max_context`` buffers, the
  normalisation round trip, and the ``[n*s]`` path ordering. It is
  reproducible because the fixtures were sampled at ``top_p = 1e-9``,
  where upstream's own filter (which always keeps the top token)
  collapses the distribution to one-hot and multinomial becomes argmax.

Skips cleanly without the optional `kronos` group or the installed
artifacts: a machine without torch must still run the rest of the suite.

Convention: full ``import x`` only. No ``from x import y``.
"""

import pathlib
import typing

import pytest

import claude_worker.kronos.install

numpy = pytest.importorskip("numpy")
torch = pytest.importorskip("torch")
claude_worker_kronos_infer = pytest.importorskip("claude_worker.kronos.infer")
infer = claude_worker_kronos_infer

import claude_worker.kronos.upstream  # noqa: E402 - after importorskip
import claude_worker.kronos.weights  # noqa: E402 - after importorskip

FIXTURES = pathlib.Path(__file__).parent / "fixtures" / "kronos"
LOGITS_EXACT = 0.0
REL_TOL = 1e-6

pytestmark = pytest.mark.kronos


def cases() -> list[pathlib.Path]:
    return sorted(FIXTURES.rglob("*.npz"))


def case_id(path: pathlib.Path) -> str:
    return "{}-{}".format(path.parent.name, path.stem)


def require(golden: typing.Any) -> tuple[dict[str, typing.Any], str, str]:
    lock = claude_worker.kronos.install.load_lock()
    name = str(golden["meta_model"][0])
    device = str(golden["meta_device"][0])
    if claude_worker.kronos.upstream.verify(lock):
        pytest.skip("kronos: upstream snapshot not installed — run the installer")
    if not claude_worker.kronos.weights.directory(name).is_dir():
        pytest.skip("kronos: {} not installed".format(name))
    if device.startswith("mps") and not torch.backends.mps.is_available():
        pytest.skip("kronos: fixtures were generated on {}, unavailable here".format(device))
    return lock, name, device


@pytest.mark.skipif(not cases(), reason="kronos: no golden fixtures")
@pytest.mark.parametrize("path", cases(), ids=case_id)
def test_adapter_matches_the_bed_goldens(path: pathlib.Path) -> None:
    golden = numpy.load(path, allow_pickle=False)
    lock, name, device = require(golden)
    # The generator's own control: if THIS was not zero, the fixture is
    # not a golden and nothing below is a gate.
    assert float(golden["eval_control_max_abs"][0]) == LOGITS_EXACT

    kronos = claude_worker.kronos.upstream.activate(lock)
    tokenizer, model, max_context = claude_worker.kronos.weights.load(name, device, lock)
    assert model.training is False
    assert tokenizer.training is False

    lookback = int(golden["meta_lookback"][0])
    norm = golden["norm"]
    with torch.no_grad():
        x = torch.from_numpy(norm).to(device)
        token = tokenizer.encode(x, half=True)
        numpy.testing.assert_array_equal(token[0].to("cpu").numpy(), golden["token_pre"])
        numpy.testing.assert_array_equal(token[1].to("cpu").numpy(), golden["token_post"])

        window = min(lookback, max_context)
        start = max(0, lookback - max_context)
        pre = token[0][:, start : start + window].contiguous()
        post = token[1][:, start : start + window].contiguous()
        stamp = (
            torch.from_numpy(golden["x_stamp"])
            .to(device)[:, start : start + window, :]
            .contiguous()
        )
        s1_logits, context = model.decode_s1(pre, post, stamp)
        s1_last = s1_logits[:, -1, :].to("cpu").numpy()
        assert numpy.abs(s1_last - golden["s1_logits_last"]).max() == LOGITS_EXACT

        sample_pre = torch.argmax(s1_logits[:, -1, :], dim=-1, keepdim=True)
        numpy.testing.assert_array_equal(sample_pre.to("cpu").numpy(), golden["sample_pre"])
        s2_logits = model.decode_s2(context, sample_pre)
        s2_last = s2_logits[:, -1, :].to("cpu").numpy()
        assert numpy.abs(s2_last - golden["s2_logits_last"]).max() == LOGITS_EXACT

        sample_post = torch.argmax(s2_logits[:, -1, :], dim=-1, keepdim=True)
        fixed_pre = torch.cat([pre, sample_pre], dim=1)
        fixed_post = torch.cat([post, sample_post], dim=1)
        tail = max(0, int(fixed_pre.shape[1]) - max_context)
        decoded = tokenizer.decode(
            [fixed_pre[:, tail:].contiguous(), fixed_post[:, tail:].contiguous()], half=True
        )
    numpy.testing.assert_allclose(
        decoded.to("cpu").numpy(), golden["decoded_fixed"], rtol=REL_TOL
    )
    # `kronos` is loaded for the same reason the adapter loads it: the
    # sampler lives on the upstream module, not on the model.
    assert hasattr(kronos, "sample_from_logits")


@pytest.mark.skipif(not cases(), reason="kronos: no golden fixtures")
@pytest.mark.parametrize("path", cases(), ids=case_id)
def test_forecaster_loop_reproduces_the_golden_paths(path: pathlib.Path) -> None:
    """The whole adapter loop, end to end, against the bed's paths.

    Deterministic only because the fixtures were drawn at
    ``top_p = 1e-9``; at any realistic ``top_p`` this comparison would be
    meaningless, which is exactly why the generator pinned it."""
    golden = numpy.load(path, allow_pickle=False)
    lock, name, device = require(golden)
    lookback = int(golden["meta_lookback"][0])
    h = int(golden["meta_h"][0])
    s = int(golden["meta_s"][0])
    top_p = float(golden["meta_top_p"][0])

    engine = infer.Forecaster(
        name,
        device,
        n_series_max=1,
        lookback=lookback,
        h_max=h,
        s_max=s,
        lock=lock,
    )
    out = numpy.zeros((1, s, h, infer.N_COLS), dtype=numpy.float64)
    engine.forecast(
        golden["ohlcv"],
        golden["x_stamp"],
        golden["y_stamp"],
        h=h,
        s=s,
        t=1.0,
        top_p=top_p,
        out=out,
    )
    numpy.testing.assert_allclose(out, golden["paths"], rtol=REL_TOL)


@pytest.mark.skipif(not cases(), reason="kronos: no golden fixtures")
def test_the_fixture_set_is_the_one_the_plan_pins() -> None:
    """Twelve files (3 checkpoints x 4 contexts) under 1 MB — the §14.1
    C0.3 acceptance, asserted so a half-copied fixture set cannot pass as
    a green gate."""
    found = cases()
    assert len(found) == 12
    assert {p.parent.name for p in found} == {"Kronos-mini", "Kronos-small", "Kronos-base"}
    assert sum(p.stat().st_size for p in found) < 1_000_000
    for path in found:
        golden = numpy.load(path, allow_pickle=False)
        assert float(golden["eval_control_max_abs"][0]) == LOGITS_EXACT
        assert str(golden["meta_torch"][0]).startswith("2.14.")
