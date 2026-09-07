# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""K1.3 — snapshot activation and checkpoint loading.

These tests build a FAKE upstream snapshot (a real directory, real
files, real hashes in a real lock) and import it for real. Nothing is
monkeypatched at the import seam, because the import seam is what is
under test: the path injection, the hash check that runs at import
rather than only at install, the refusal to shadow a foreign top-level
``model`` package, and the ``.eval()`` discipline upstream lacks.

No torch: the fake ``model.kronos`` is plain Python, which is exactly
what makes these adapter behaviours testable on a machine without the
optional ``kronos`` dependency group.

Convention: full ``import x`` only. No ``from x import y``.
"""

import hashlib
import importlib
import pathlib
import sys
import typing

import pytest

import claude_worker.kronos.install
import claude_worker.kronos.upstream
import claude_worker.kronos.weights

INIT_PY: str = "import model.kronos\n"
KRONOS_PY: str = '''
"""Fake upstream, shaped like the real one at the seams we touch."""


class _Module:
    def __init__(self, where):
        self.where = where
        self.training = True
        self.device = None

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
    pass


class Kronos(_Module):
    pass


def sample_from_logits(logits, temperature=1.0, top_k=None, top_p=None, sample_logits=True):
    return logits
'''
# A checkpoint whose .eval() silently does nothing — the shape of a future
# upstream refactor that re-enters training mode without saying so.
NEVER_EVAL: str = KRONOS_PY.replace(
    "class Kronos(_Module):\n    pass",
    "class Kronos(_Module):\n    def eval(self):\n        return self",
)
LICENSE_TXT: str = "MIT License\n\nCopyright (c) fake upstream\n"
BLOB: bytes = b"weights-bytes" * 8
CFG: bytes = b'{"d_model": 8}\n'


def sha(payload: bytes) -> str:
    return hashlib.sha256(payload).hexdigest()


def lock_text(kronos_py: str) -> str:
    return f"""
[upstream]
repo    = "fake/Kronos"
commit  = "0123456789abcdef0123456789abcdef01234567"
license = "MIT"

[[upstream.file]]
path   = "model/__init__.py"
url    = "https://example.invalid/model/__init__.py"
sha256 = "{sha(INIT_PY.encode())}"
bytes  = {len(INIT_PY.encode())}

[[upstream.file]]
path   = "model/kronos.py"
url    = "https://example.invalid/model/kronos.py"
sha256 = "{sha(kronos_py.encode())}"
bytes  = {len(kronos_py.encode())}

[[upstream.file]]
path   = "LICENSE"
url    = "https://example.invalid/LICENSE"
sha256 = "{sha(LICENSE_TXT.encode())}"
bytes  = {len(LICENSE_TXT.encode())}

[weights."Fake-model"]
kind        = "model"
repo        = "fake/model"
revision    = "cafebabe"
max_context = 16
tokenizer   = "Fake-tokenizer"

[[weights."Fake-model".file]]
path   = "model.safetensors"
url    = "https://example.invalid/w/model.safetensors"
sha256 = "{sha(BLOB)}"
bytes  = {len(BLOB)}

[weights."Fake-tokenizer"]
kind     = "tokenizer"
repo     = "fake/tokenizer"
revision = "d00dd00d"

[[weights."Fake-tokenizer".file]]
path   = "config.json"
url    = "https://example.invalid/t/config.json"
sha256 = "{sha(CFG)}"
bytes  = {len(CFG)}

[weights."Fake-tokenizer-only"]
kind     = "tokenizer"
repo     = "fake/tokenizer"
revision = "d00dd00d"

[[weights."Fake-tokenizer-only".file]]
path   = "config.json"
url    = "https://example.invalid/t/config.json"
sha256 = "{sha(CFG)}"
bytes  = {len(CFG)}
"""


def drop_model_modules() -> None:
    for name in [key for key in sys.modules if key == "model" or key.startswith("model.")]:
        del sys.modules[name]


@pytest.fixture(autouse=True)
def clean_import_state() -> typing.Iterator[None]:
    """`model` is a spectacularly generic name; leaking one between tests
    would make the shadowing test pass for the wrong reason.

    Cleared BEFORE as well as after: the parity suite activates the real
    snapshot and sorts earlier alphabetically, so a fixture that only
    tidies up after itself lets a real `model` leak in and fail these
    fake-snapshot tests for a reason that has nothing to do with them."""
    before = list(sys.path)
    drop_model_modules()
    yield
    drop_model_modules()
    sys.path[:] = before


@pytest.fixture
def bed(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> typing.Callable[..., dict[str, typing.Any]]:
    def build(*, kronos_py: str = KRONOS_PY, install_weights: bool = True) -> dict[str, typing.Any]:
        monkeypatch.setenv(claude_worker.kronos.install.VENDOR_ENV, str(tmp_path / "vendor"))
        monkeypatch.setenv(claude_worker.kronos.install.ARTIFACTS_ENV, str(tmp_path / "artifacts"))
        lock_file = tmp_path / "kronos.lock"
        lock_file.write_text(lock_text(kronos_py), encoding="utf-8")
        lock = claude_worker.kronos.install.load_lock(lock_file)
        tree = claude_worker.kronos.install.vendor_dir(lock)
        (tree / "model").mkdir(parents=True)
        (tree / "model" / "__init__.py").write_text(INIT_PY, encoding="utf-8")
        (tree / "model" / "kronos.py").write_text(kronos_py, encoding="utf-8")
        (tree / "LICENSE").write_text(LICENSE_TXT, encoding="utf-8")
        if install_weights:
            model_dir = claude_worker.kronos.install.weights_dir("Fake-model")
            model_dir.mkdir(parents=True)
            (model_dir / "model.safetensors").write_bytes(BLOB)
            tok_dir = claude_worker.kronos.install.weights_dir("Fake-tokenizer")
            tok_dir.mkdir(parents=True)
            (tok_dir / "config.json").write_bytes(CFG)
        return lock

    return build


# ---- activate ------------------------------------------------------------


def test_activate_imports_the_pinned_snapshot(
    bed: typing.Callable[..., dict[str, typing.Any]]
) -> None:
    lock = bed()
    module = claude_worker.kronos.upstream.activate(lock)
    assert hasattr(module, "KronosTokenizer")
    tree = claude_worker.kronos.upstream.vendor_dir(lock)
    assert str(tree) in sys.path
    assert str(module.__file__).startswith(str(tree))
    # Idempotent: a second call returns the same module object.
    assert claude_worker.kronos.upstream.activate(lock) is module


def test_activate_refuses_one_altered_byte(
    bed: typing.Callable[..., dict[str, typing.Any]]
) -> None:
    """The snapshot lives outside git in a writable directory and nothing
    else checks it at run time; if a changed file could still import,
    "upstream as-is, pinned" would be true only at install time."""
    lock = bed()
    target = claude_worker.kronos.upstream.vendor_dir(lock) / "model" / "kronos.py"
    target.write_text(KRONOS_PY + "\nBACKDOOR = 1\n", encoding="utf-8")
    with pytest.raises(claude_worker.kronos.upstream.UpstreamError) as caught:
        claude_worker.kronos.upstream.activate(lock)
    assert "model/kronos.py" in str(caught.value)
    assert "model" not in sys.modules


def test_activate_refuses_when_a_foreign_model_package_is_imported(
    bed: typing.Callable[..., dict[str, typing.Any]], tmp_path: pathlib.Path
) -> None:
    lock = bed()
    other = tmp_path / "elsewhere"
    (other / "model").mkdir(parents=True)
    (other / "model" / "__init__.py").write_text("MARKER = 'not ours'\n", encoding="utf-8")
    sys.path.insert(0, str(other))
    # importlib, not an `import model` statement: the import must happen
    # HERE, after the path is rigged, and a statement at this point in a
    # function is what PLC0415 exists to catch.
    assert importlib.import_module("model").MARKER == "not ours"

    with pytest.raises(claude_worker.kronos.upstream.UpstreamError) as caught:
        claude_worker.kronos.upstream.activate(lock)
    assert "already imported" in str(caught.value)


def test_activate_says_what_to_run_when_nothing_is_installed(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv(claude_worker.kronos.install.VENDOR_ENV, str(tmp_path / "empty"))
    lock_file = tmp_path / "kronos.lock"
    lock_file.write_text(lock_text(KRONOS_PY), encoding="utf-8")
    lock = claude_worker.kronos.install.load_lock(lock_file)
    with pytest.raises(claude_worker.kronos.upstream.UpstreamError) as caught:
        claude_worker.kronos.upstream.activate(lock)
    assert "claude_worker.kronos install" in str(caught.value)


def test_license_text_is_the_snapshots_own(
    bed: typing.Callable[..., dict[str, typing.Any]]
) -> None:
    lock = bed()
    assert "MIT License" in claude_worker.kronos.upstream.license_text(lock)


# ---- weights.load --------------------------------------------------------


def test_load_puts_both_modules_in_eval_mode(
    bed: typing.Callable[..., dict[str, typing.Any]]
) -> None:
    """Upstream never calls .eval(): `small` crashes on MPS without it and
    every model silently samples with dropout live on CPU."""
    lock = bed()
    tokenizer, model, max_context = claude_worker.kronos.weights.load("Fake-model", "cpu", lock)
    assert max_context == 16
    assert tokenizer.training is False
    assert model.training is False
    assert tokenizer.device == "cpu"
    assert model.device == "cpu"
    assert str(model.where).endswith("Fake-model")
    assert str(tokenizer.where).endswith("Fake-tokenizer")


def test_load_raises_if_eval_does_not_take(
    bed: typing.Callable[..., dict[str, typing.Any]]
) -> None:
    lock = bed(kronos_py=NEVER_EVAL)
    with pytest.raises(claude_worker.kronos.weights.WeightsError) as caught:
        claude_worker.kronos.weights.load("Fake-model", "cpu", lock)
    assert "training mode" in str(caught.value)


def test_load_refuses_altered_weights(
    bed: typing.Callable[..., dict[str, typing.Any]]
) -> None:
    lock = bed()
    blob = claude_worker.kronos.install.weights_dir("Fake-model") / "model.safetensors"
    payload = bytearray(blob.read_bytes())
    payload[0] ^= 0xFF
    blob.write_bytes(bytes(payload))
    with pytest.raises(claude_worker.kronos.weights.WeightsError) as caught:
        claude_worker.kronos.weights.load("Fake-model", "cpu", lock)
    assert "REFUSING" in str(caught.value)


def test_load_refuses_a_tokenizer_as_a_model(
    bed: typing.Callable[..., dict[str, typing.Any]]
) -> None:
    lock = bed()
    with pytest.raises(claude_worker.kronos.weights.WeightsError):
        claude_worker.kronos.weights.load("Fake-tokenizer-only", "cpu", lock)


def test_load_refuses_an_unknown_checkpoint(
    bed: typing.Callable[..., dict[str, typing.Any]]
) -> None:
    lock = bed()
    with pytest.raises(claude_worker.kronos.weights.WeightsError):
        claude_worker.kronos.weights.load("Kronos-enormous", "cpu", lock)


def test_load_tells_you_to_install_a_missing_checkpoint(
    bed: typing.Callable[..., dict[str, typing.Any]]
) -> None:
    lock = bed(install_weights=False)
    with pytest.raises(claude_worker.kronos.weights.WeightsError) as caught:
        claude_worker.kronos.weights.load("Fake-model", "cpu", lock)
    assert "--model Fake-model" in str(caught.value)
