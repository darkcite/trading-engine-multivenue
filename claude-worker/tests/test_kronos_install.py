# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""K1.2 — the pinned-artifact installer.

The interesting tests here are the refusals. The whole point of
``kronos.lock`` is that a wrong byte can never be observed as installed:
the sidecar loads whatever sits in ``~/multivenue/vendor/kronos`` and
``~/multivenue/artifacts/kronos`` without asking the network again, so if
a torn download could survive, every forecast after it would be silently
running unknown code or unknown weights.

Every test runs against ``httpx.MockTransport``. No socket, no network
mark needed.

Convention: full ``import x`` only. No ``from x import y``.
"""

import hashlib
import json
import pathlib
import typing

import httpx
import pytest

import claude_worker.kronos.install

UPSTREAM_A: bytes = b"# model/__init__.py\nversion = 1\n"
UPSTREAM_B: bytes = b"MIT License\n\nCopyright (c) upstream\n"
WEIGHT_BLOB: bytes = b"\x00\x01\x02\x03" * 64
WEIGHT_CFG: bytes = b'{"d_model": 8}\n'


def sha(payload: bytes) -> str:
    return hashlib.sha256(payload).hexdigest()


def write_lock(tmp_path: pathlib.Path, *, break_hash: bool = False) -> pathlib.Path:
    blob_sha = sha(WEIGHT_BLOB) if not break_hash else sha(b"not the bytes we will serve")
    text = f"""
[upstream]
repo    = "example/Kronos"
commit  = "0123456789abcdef0123456789abcdef01234567"
license = "MIT"

[[upstream.file]]
path   = "model/__init__.py"
url    = "https://example.invalid/upstream/model/__init__.py"
sha256 = "{sha(UPSTREAM_A)}"
bytes  = {len(UPSTREAM_A)}

[[upstream.file]]
path   = "LICENSE"
url    = "https://example.invalid/upstream/LICENSE"
sha256 = "{sha(UPSTREAM_B)}"
bytes  = {len(UPSTREAM_B)}

[weights."Fake-model"]
kind        = "model"
repo        = "example/Fake-model"
revision    = "cafebabe"
max_context = 512
tokenizer   = "Fake-tokenizer"

[[weights."Fake-model".file]]
path   = "model.safetensors"
url    = "https://example.invalid/weights/model.safetensors"
sha256 = "{blob_sha}"
bytes  = {len(WEIGHT_BLOB)}

[[weights."Fake-model".file]]
path   = "config.json"
url    = "https://example.invalid/weights/config.json"
sha256 = "{sha(WEIGHT_CFG)}"
bytes  = {len(WEIGHT_CFG)}

[weights."Fake-tokenizer"]
kind     = "tokenizer"
repo     = "example/Fake-tokenizer"
revision = "d00d"

[[weights."Fake-tokenizer".file]]
path   = "config.json"
url    = "https://example.invalid/tokenizer/config.json"
sha256 = "{sha(WEIGHT_CFG)}"
bytes  = {len(WEIGHT_CFG)}
"""
    path = tmp_path / "kronos.lock"
    path.write_text(text, encoding="utf-8")
    return path


BODIES: dict[str, bytes] = {
    "https://example.invalid/upstream/model/__init__.py": UPSTREAM_A,
    "https://example.invalid/upstream/LICENSE": UPSTREAM_B,
    "https://example.invalid/weights/model.safetensors": WEIGHT_BLOB,
    "https://example.invalid/weights/config.json": WEIGHT_CFG,
    "https://example.invalid/tokenizer/config.json": WEIGHT_CFG,
}


class Counter:
    def __init__(self) -> None:
        self.n: int = 0
        self.urls: list[str] = []


def mock_client(counter: Counter, *, status: int = 200, boom: bool = False) -> httpx.Client:
    def handler(request: httpx.Request) -> httpx.Response:
        counter.n += 1
        counter.urls.append(str(request.url))
        if boom:
            raise httpx.ConnectError("no route", request=request)
        body = BODIES.get(str(request.url))
        if body is None:
            return httpx.Response(404)
        return httpx.Response(status, content=body)

    return httpx.Client(transport=httpx.MockTransport(handler))


@pytest.fixture
def dirs(
    tmp_path: pathlib.Path, monkeypatch: pytest.MonkeyPatch
) -> tuple[pathlib.Path, pathlib.Path]:
    vendor = tmp_path / "vendor"
    artifacts = tmp_path / "artifacts"
    monkeypatch.setenv(claude_worker.kronos.install.VENDOR_ENV, str(vendor))
    monkeypatch.setenv(claude_worker.kronos.install.ARTIFACTS_ENV, str(artifacts))
    return vendor, artifacts


def quiet(_line: str) -> None:
    return None


# ---- the committed lock --------------------------------------------------


def test_repo_lock_parses_and_pins_what_the_plan_names() -> None:
    lock = claude_worker.kronos.install.load_lock()
    upstream = typing.cast(dict[str, typing.Any], lock["upstream"])
    assert upstream["repo"] == "shiyu-coder/Kronos"
    assert upstream["commit"] == "67b630e67f6a18c9e9be918d9b4337c960db1e9a"
    assert upstream["license"] == "MIT"
    paths = {str(f["path"]) for f in claude_worker.kronos.install._files_of(upstream)}
    assert paths == {"model/__init__.py", "model/kronos.py", "model/module.py", "LICENSE"}
    weights = typing.cast(dict[str, typing.Any], lock["weights"])
    assert set(weights) == {
        "Kronos-base",
        "Kronos-small",
        "Kronos-mini",
        "Kronos-Tokenizer-base",
        "Kronos-Tokenizer-2k",
    }
    # The §3a.2 byte counts — a transcription slip here would only surface
    # as a refused download hours later.
    sizes = {
        name: {str(f["path"]): int(f["bytes"]) for f in claude_worker.kronos.install._files_of(e)}
        for name, e in weights.items()
    }
    assert sizes["Kronos-mini"]["model.safetensors"] == 16440776
    assert sizes["Kronos-small"]["model.safetensors"] == 98980656
    assert sizes["Kronos-base"]["model.safetensors"] == 409264008
    assert sizes["Kronos-Tokenizer-2k"]["model.safetensors"] == 15842376
    assert sizes["Kronos-Tokenizer-base"]["model.safetensors"] == 15842368
    assert weights["Kronos-base"]["tokenizer"] == "Kronos-Tokenizer-base"
    assert weights["Kronos-base"]["max_context"] == 512
    assert weights["Kronos-mini"]["tokenizer"] == "Kronos-Tokenizer-2k"
    assert weights["Kronos-mini"]["max_context"] == 2048


def test_repo_lock_urls_are_immutable_pins() -> None:
    """A `resolve/main` weight URL or a branch-named raw URL is not a pin:
    the bytes behind it can change, which turns a reinstall into a refusal
    instead of a byte-identical fetch."""
    lock = claude_worker.kronos.install.load_lock()
    upstream = typing.cast(dict[str, typing.Any], lock["upstream"])
    commit = str(upstream["commit"])
    for entry in claude_worker.kronos.install._files_of(upstream):
        assert str(entry["url"]).startswith(f"https://raw.githubusercontent.com/{upstream['repo']}/")
        assert commit in str(entry["url"])
    weights = typing.cast(dict[str, typing.Any], lock["weights"])
    for name, raw in weights.items():
        entry = typing.cast(dict[str, typing.Any], raw)
        revision = str(entry["revision"])
        assert len(revision) == 40, name
        for item in claude_worker.kronos.install._files_of(entry):
            assert f"/resolve/{revision}/" in str(item["url"]), name


# ---- content addressing --------------------------------------------------


def test_upstream_id_ignores_formatting_but_not_content(tmp_path: pathlib.Path) -> None:
    path = write_lock(tmp_path)
    first = claude_worker.kronos.install.upstream_id(
        claude_worker.kronos.install.load_lock(path)
    )
    path.write_text("# a new comment\n" + path.read_text(encoding="utf-8"), encoding="utf-8")
    assert (
        claude_worker.kronos.install.upstream_id(claude_worker.kronos.install.load_lock(path))
        == first
    )
    path.write_text(
        path.read_text(encoding="utf-8").replace(sha(UPSTREAM_A), sha(b"other")), encoding="utf-8"
    )
    assert (
        claude_worker.kronos.install.upstream_id(claude_worker.kronos.install.load_lock(path))
        != first
    )


# ---- install -------------------------------------------------------------


def test_install_writes_everything_then_is_a_no_op(
    tmp_path: pathlib.Path, dirs: tuple[pathlib.Path, pathlib.Path]
) -> None:
    vendor, artifacts = dirs
    lock = claude_worker.kronos.install.load_lock(write_lock(tmp_path))
    counter = Counter()
    with mock_client(counter) as client:
        code = claude_worker.kronos.install.run_install(lock, client, "all", None, quiet)
    assert code == claude_worker.kronos.install.RC_OK
    assert counter.n == 5
    tree = claude_worker.kronos.install.vendor_dir(lock)
    assert tree.parent == vendor
    assert (tree / "model" / "__init__.py").read_bytes() == UPSTREAM_A
    assert (tree / "LICENSE").read_bytes() == UPSTREAM_B
    assert (artifacts / "Fake-model" / "model.safetensors").read_bytes() == WEIGHT_BLOB
    manifest = json.loads(
        (artifacts / "Fake-model" / claude_worker.kronos.install.MANIFEST_NAME).read_text(
            encoding="utf-8"
        )
    )
    assert manifest["files"]["model.safetensors"] == sha(WEIGHT_BLOB)
    assert manifest["revision"] == "cafebabe"
    # Second run: not one request.
    again = Counter()
    with mock_client(again) as client:
        code = claude_worker.kronos.install.run_install(lock, client, "all", None, quiet)
    assert code == claude_worker.kronos.install.RC_OK
    assert again.n == 0


def test_install_refuses_a_wrong_hash_and_writes_nothing(
    tmp_path: pathlib.Path, dirs: tuple[pathlib.Path, pathlib.Path]
) -> None:
    _vendor, artifacts = dirs
    lock = claude_worker.kronos.install.load_lock(write_lock(tmp_path, break_hash=True))
    counter = Counter()
    lines: list[str] = []
    with mock_client(counter) as client:
        code = claude_worker.kronos.install.run_install(
            lock, client, "weights", ["Fake-model"], lines.append
        )
    assert code == claude_worker.kronos.install.RC_MISMATCH
    assert not (artifacts / "Fake-model" / "model.safetensors").exists()
    assert not (artifacts / "Fake-model" / "model.safetensors.part").exists()
    assert not (artifacts / "Fake-model" / claude_worker.kronos.install.MANIFEST_NAME).exists()
    assert any("REFUSED" in line for line in lines)


def test_transport_failure_is_its_own_code(
    tmp_path: pathlib.Path, dirs: tuple[pathlib.Path, pathlib.Path]
) -> None:
    lock = claude_worker.kronos.install.load_lock(write_lock(tmp_path))
    counter = Counter()
    with mock_client(counter, boom=True) as client:
        code = claude_worker.kronos.install.run_install(lock, client, "upstream", None, quiet)
    assert code == claude_worker.kronos.install.RC_TRANSPORT
    counter = Counter()
    with mock_client(counter, status=503) as client:
        code = claude_worker.kronos.install.run_install(lock, client, "upstream", None, quiet)
    assert code == claude_worker.kronos.install.RC_TRANSPORT


def test_model_selection_pulls_its_tokenizer_along(
    tmp_path: pathlib.Path, dirs: tuple[pathlib.Path, pathlib.Path]
) -> None:
    _vendor, artifacts = dirs
    lock = claude_worker.kronos.install.load_lock(write_lock(tmp_path))
    counter = Counter()
    with mock_client(counter) as client:
        code = claude_worker.kronos.install.run_install(
            lock, client, "weights", ["Fake-model"], quiet
        )
    assert code == claude_worker.kronos.install.RC_OK
    assert (artifacts / "Fake-tokenizer" / "config.json").exists()


def test_unknown_model_name_is_a_lock_error(tmp_path: pathlib.Path) -> None:
    lock = claude_worker.kronos.install.load_lock(write_lock(tmp_path))
    with pytest.raises(claude_worker.kronos.install.LockError):
        claude_worker.kronos.install.weight_names(lock, ["Kronos-enormous"])


# ---- verify --------------------------------------------------------------


def test_verify_catches_one_flipped_byte(
    tmp_path: pathlib.Path, dirs: tuple[pathlib.Path, pathlib.Path]
) -> None:
    _vendor, artifacts = dirs
    lock = claude_worker.kronos.install.load_lock(write_lock(tmp_path))
    with mock_client(Counter()) as client:
        assert (
            claude_worker.kronos.install.run_install(lock, client, "all", None, quiet)
            == claude_worker.kronos.install.RC_OK
        )
    ok = claude_worker.kronos.install.RC_OK
    assert claude_worker.kronos.install.run_verify(lock, quiet) == ok
    blob = artifacts / "Fake-model" / "model.safetensors"
    payload = bytearray(blob.read_bytes())
    payload[7] ^= 0xFF
    blob.write_bytes(bytes(payload))
    assert (
        claude_worker.kronos.install.run_verify(lock, quiet)
        == claude_worker.kronos.install.RC_MISMATCH
    )


def test_check_installed_names_a_truncated_file(
    tmp_path: pathlib.Path, dirs: tuple[pathlib.Path, pathlib.Path]
) -> None:
    _vendor, artifacts = dirs
    lock = claude_worker.kronos.install.load_lock(write_lock(tmp_path))
    with mock_client(Counter()) as client:
        claude_worker.kronos.install.run_install(lock, client, "weights", ["Fake-model"], quiet)
    blob = artifacts / "Fake-model" / "model.safetensors"
    blob.write_bytes(blob.read_bytes()[:-4])
    weights = typing.cast(dict[str, typing.Any], lock["weights"])
    entry = typing.cast(dict[str, typing.Any], weights["Fake-model"])
    bad = claude_worker.kronos.install.check_installed(
        artifacts / "Fake-model", claude_worker.kronos.install._files_of(entry)
    )
    assert bad == ["model.safetensors"]


# ---- lock errors ---------------------------------------------------------


@pytest.mark.parametrize(
    "text",
    [
        "",
        "[upstream]\nrepo='x'\n",
        '[upstream]\nrepo="x"\ncommit="y"\nlicense="MIT"\n',
        '[upstream]\nrepo="x"\ncommit="y"\nlicense="MIT"\n'
        '[[upstream.file]]\npath="p"\nurl="u"\nsha256="s"\nbytes=1\n',
    ],
)
def test_malformed_lock_is_refused(tmp_path: pathlib.Path, text: str) -> None:
    path = tmp_path / "kronos.lock"
    path.write_text(text, encoding="utf-8")
    with pytest.raises(claude_worker.kronos.install.LockError):
        claude_worker.kronos.install.load_lock(path)


def test_main_returns_five_on_a_missing_lock(tmp_path: pathlib.Path) -> None:
    code = claude_worker.kronos.install.main(
        ["verify", "--lock", str(tmp_path / "nope.lock")]
    )
    assert code == claude_worker.kronos.install.RC_LOCK
