# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""Gate G-S1 — the SigV4 signer and the object-store verbs.

The vector test is the important one. It exercises the PURE signer against 11
botocore-derived golden vectors and asserts each of the four stages separately,
so a regression names its own stage instead of arriving as an unexplained 403
from a live endpoint months later.

If the signer disagrees with a vector, **the signer is wrong**. The fixture is
not to be edited to make a test pass.

Everything else runs over `tests/tests.fake_s3.py` — an httpx.MockTransport. No test
in this file can open a socket.

Convention: full ``import x`` only. No ``from x import y``.
"""

import hashlib
import hmac
import json
import pathlib
import typing
import urllib.parse

import httpx
import pytest

import claude_worker.objstore
import tests.fake_s3

FIXTURE = pathlib.Path(__file__).parent / "fixtures" / "sigv4" / "vectors.json"


def load_vectors() -> dict[str, typing.Any]:
    return typing.cast(dict[str, typing.Any], json.loads(FIXTURE.read_text(encoding="utf-8")))


def vector_ids() -> list[str]:
    return [v["name"] for v in load_vectors()["vectors"]]


# --------------------------------------------------------------------------
# the golden vectors
# --------------------------------------------------------------------------


@pytest.mark.parametrize("index", range(len(load_vectors()["vectors"])), ids=vector_ids())
@pytest.mark.parametrize(
    "stage", ["canonical_request", "string_to_sign", "signature", "authorization"]
)
def test_sigv4_vectors_match_fixture(index: int, stage: str) -> None:
    doc = load_vectors()
    vector = doc["vectors"][index]

    pairs: list[tuple[str, str]] = []
    if vector["query"]:
        for chunk in vector["query"].split("&"):
            name, _, value = chunk.partition("=")
            pairs.append((urllib.parse.unquote(name), urllib.parse.unquote(value)))

    headers: dict[str, str] = {"host": vector["host"]}
    for name, value in vector.get("extra_headers", {}).items():
        headers[name.lower()] = value
    headers["x-amz-content-sha256"] = vector["x_amz_content_sha256"]
    headers["x-amz-date"] = vector["x_amz_date"]

    got = claude_worker.objstore.sign(
        method=vector["method"],
        path=vector["path"],
        query=claude_worker.objstore.canonical_query(pairs),
        headers=headers,
        payload_sha256_hex=vector["x_amz_content_sha256"],
        amz_date=vector["x_amz_date"],
        creds=claude_worker.objstore.Credentials(
            access_key_id=doc["access_key_id"], secret_access_key=doc["secret_access_key"]
        ),
        region=doc["region"],
    )
    assert getattr(got, stage) == vector[stage], f"{vector['name']}: {stage} mismatch"


def test_signing_key_chain_matches_fixture() -> None:
    doc = load_vectors()
    key = ("AWS4" + doc["secret_access_key"]).encode("utf-8")
    for part, label in (
        (doc["x_amz_date"][:8], "kDate"),
        (doc["region"], "kRegion"),
        (doc["service"], "kService"),
        ("aws4_request", "kSigning"),
    ):
        key = hmac.new(key, part.encode("utf-8"), hashlib.sha256).digest()
        assert key.hex() == doc["signing_key_chain_hex"][label], label


def test_canonical_query_encodes_and_sorts() -> None:
    # A continuation token really does contain '/', '+' and '='.
    query = claude_worker.objstore.canonical_query(
        [("prefix", "v1/runs/mbp-m4/"), ("continuation-token", "1/abc+def="), ("list-type", "2")]
    )
    assert query == (
        "continuation-token=1%2Fabc%2Bdef%3D&list-type=2&prefix=v1%2Fruns%2Fmbp-m4%2F"
    )


def test_canonical_query_space_is_percent_20_never_plus() -> None:
    assert claude_worker.objstore.canonical_query([("k", "a b")]) == "k=a%20b"


def test_valueless_subresource_is_name_equals() -> None:
    assert claude_worker.objstore.canonical_query([("uploads", "")]) == "uploads="


# --------------------------------------------------------------------------
# the verbs, over the fake
# --------------------------------------------------------------------------


def make_store(
    fake: tests.fake_s3.FakeS3, *, addressing: str = "virtual"
) -> claude_worker.objstore.ObjectStore:
    endpoint = claude_worker.objstore.Endpoint(
        host="fsn1.your-objectstorage.com",
        region="fsn1",
        bucket=fake.bucket,
        addressing=addressing,
    )
    creds = claude_worker.objstore.Credentials(
        access_key_id="AKIAEXAMPLEARCHIVE00", secret_access_key="archive-golden-secret"
    )
    return claude_worker.objstore.ObjectStore(
        endpoint, creds, fake.client(), timeout_s=5.0, sleep=lambda _s: None
    )


def test_put_bytes_head_and_get_bytes_round_trip() -> None:
    fake = tests.fake_s3.FakeS3()
    store = make_store(fake)
    meta = store.put_bytes("v1/index/mbp-m4/run-1.json", b'{"x":1}')
    assert meta.etag == hashlib.md5(b'{"x":1}').hexdigest()
    head = store.head("v1/index/mbp-m4/run-1.json")
    assert head is not None and head.size == 7
    assert store.get_bytes("v1/index/mbp-m4/run-1.json") == b'{"x":1}'


def test_head_missing_key_is_none_not_error() -> None:
    fake = tests.fake_s3.FakeS3()
    assert make_store(fake).head("v1/runs/mbp-m4/nope/_manifest.json") is None


def test_put_and_get_file_round_trip(tmp_path: pathlib.Path) -> None:
    fake = tests.fake_s3.FakeS3()
    store = make_store(fake)
    src = tmp_path / "pm-ticks.pmlr.zst"
    src.write_bytes(b"pmlr-payload" * 1000)
    store.put("v1/runs/mbp-m4/run-1/pm-ticks.pmlr.zst", src)
    dst = tmp_path / "pulled.zst"
    store.get("v1/runs/mbp-m4/run-1/pm-ticks.pmlr.zst", dst, expect_size=src.stat().st_size)
    assert dst.read_bytes() == src.read_bytes()


def test_get_truncated_response_refuses_to_write_destination(tmp_path: pathlib.Path) -> None:
    fake = tests.fake_s3.FakeS3()
    store = make_store(fake)
    src = tmp_path / "obj.bin"
    src.write_bytes(b"x" * 4096)
    store.put("v1/runs/mbp-m4/run-1/obj.bin", src)
    fake.truncate_get.add("v1/runs/mbp-m4/run-1/obj.bin")
    dst = tmp_path / "pulled.bin"
    with pytest.raises(claude_worker.objstore.ObjStoreError, match="truncated"):
        store.get("v1/runs/mbp-m4/run-1/obj.bin", dst, expect_size=4096)
    assert not dst.exists()
    assert not dst.with_name(dst.name + ".part").exists()


def test_put_etag_mismatch_raises_digest_mismatch(tmp_path: pathlib.Path) -> None:
    fake = tests.fake_s3.FakeS3()
    fake.corrupt_etag.add("v1/runs/mbp-m4/run-1/obj.bin")
    store = make_store(fake)
    src = tmp_path / "obj.bin"
    src.write_bytes(b"y" * 128)
    with pytest.raises(claude_worker.objstore.DigestMismatch):
        store.put("v1/runs/mbp-m4/run-1/obj.bin", src)


def test_list_paginates_and_returns_every_key() -> None:
    fake = tests.fake_s3.FakeS3()
    store = make_store(fake)
    for i in range(7):
        store.put_bytes(f"v1/index/mbp-m4/run-{i}.json", b"{}")
    keys = [m.key for m in store.list("v1/index/mbp-m4/", page_size=2)]
    assert keys == [f"v1/index/mbp-m4/run-{i}.json" for i in range(7)]


def test_list_parses_namespaced_xml() -> None:
    """The regression guard for the S0 finding.

    Element.iter() ignores the '{*}' wildcard, so a parser written with `iter`
    returns an EMPTY listing against namespaced XML — silently. The fake serves
    namespaced XML precisely so this can never regress unnoticed.
    """
    fake = tests.fake_s3.FakeS3()
    store = make_store(fake)
    store.put_bytes("v1/index/mbp-m4/run-1.json", b"{}")
    found = list(store.list("v1/index/mbp-m4/"))
    assert len(found) == 1, "namespaced ListBucketResult was parsed as empty"
    assert found[0].size == 2


def test_list_prefixes_returns_common_prefixes() -> None:
    fake = tests.fake_s3.FakeS3()
    store = make_store(fake)
    for run in ("run-1", "run-2", "run-3"):
        store.put_bytes(f"v1/runs/mbp-m4/{run}/_manifest.json", b"{}")
    assert store.list_prefixes("v1/runs/mbp-m4/") == [
        "v1/runs/mbp-m4/run-1/",
        "v1/runs/mbp-m4/run-2/",
        "v1/runs/mbp-m4/run-3/",
    ]


def test_list_start_after_is_exclusive() -> None:
    fake = tests.fake_s3.FakeS3()
    store = make_store(fake)
    for i in range(4):
        store.put_bytes(f"v1/index/mbp-m4/run-{i}.json", b"{}")
    keys = [m.key for m in store.list("v1/index/mbp-m4/", start_after="v1/index/mbp-m4/run-1.json")]
    assert keys == ["v1/index/mbp-m4/run-2.json", "v1/index/mbp-m4/run-3.json"]


def test_multipart_put_round_trip_and_composite_etag(tmp_path: pathlib.Path) -> None:
    fake = tests.fake_s3.FakeS3()
    store = make_store(fake)
    part = 5 * 1024 * 1024
    src = tmp_path / "bn-ticks.pmlr.zst"
    src.write_bytes(b"a" * (part * 2 + 1024))
    meta = store.multipart_put("v1/runs/mbp-m4/run-1/bn-ticks.pmlr.zst", src, part_size=part)
    assert meta.etag.endswith("-3")
    assert fake.objects["v1/runs/mbp-m4/run-1/bn-ticks.pmlr.zst"] == src.read_bytes()


def test_multipart_refuses_part_size_below_the_5mib_floor(tmp_path: pathlib.Path) -> None:
    fake = tests.fake_s3.FakeS3()
    store = make_store(fake)
    src = tmp_path / "obj.bin"
    src.write_bytes(b"a" * 4096)
    with pytest.raises(claude_worker.objstore.ObjStoreError, match="minimum"):
        store.multipart_put("v1/runs/mbp-m4/run-1/obj.bin", src, part_size=1024 * 1024)


def test_multipart_composite_mismatch_aborts_the_upload(tmp_path: pathlib.Path) -> None:
    fake = tests.fake_s3.FakeS3()
    key = "v1/runs/mbp-m4/run-1/obj.bin"
    fake.corrupt_etag.add(key)
    store = make_store(fake)
    part = 5 * 1024 * 1024
    src = tmp_path / "obj.bin"
    src.write_bytes(b"b" * (part + 16))
    with pytest.raises(claude_worker.objstore.DigestMismatch):
        store.multipart_put(key, src, part_size=part)


def test_abort_multipart_is_the_only_delete_verb_and_is_best_effort() -> None:
    fake = tests.fake_s3.FakeS3()
    store = make_store(fake)
    store.abort_multipart("v1/runs/mbp-m4/run-1/obj.bin", "2~does-not-exist")
    assert all(m != "DELETE" or "uploadId" in q for m, _p, q in fake.requests)


def test_client_exposes_no_delete_object() -> None:
    """S-LAW 11 is structural, not a promise in a docstring."""
    assert not hasattr(claude_worker.objstore.ObjectStore, "delete_object")
    assert not hasattr(claude_worker.objstore.ObjectStore, "presign")
    assert not hasattr(claude_worker.objstore.ObjectStore, "put_lifecycle")


def test_retries_on_503_then_succeeds() -> None:
    fake = tests.fake_s3.FakeS3()
    fake.status_once["GET"] = 503
    store = make_store(fake)
    store.put_bytes("v1/index/mbp-m4/run-1.json", b"{}")
    assert store.get_bytes("v1/index/mbp-m4/run-1.json") == b"{}"
    assert fake.count("GET") == 2


def test_does_not_retry_on_403() -> None:
    fake = tests.fake_s3.FakeS3()
    fake.status_once["HEAD"] = 403
    store = make_store(fake)
    with pytest.raises(claude_worker.objstore.ObjStoreError) as caught:
        store.head("v1/index/mbp-m4/run-1.json")
    assert caught.value.status == 403
    assert fake.count("HEAD") == 1, "a 403 must fail loudly, not be retried"


def test_resigns_on_every_retry_attempt() -> None:
    """A retry that reused the first signature would fail exactly when the
    network is worst — x-amz-date must be within 15 minutes of server time."""
    fake = tests.fake_s3.FakeS3()
    fake.status_once["GET"] = 503
    seen: list[str] = []

    def record(request: httpx.Request) -> httpx.Response:
        seen.append(request.headers["authorization"])
        return fake.handle(request)

    endpoint = claude_worker.objstore.Endpoint(
        host="fsn1.your-objectstorage.com", region="fsn1", bucket=fake.bucket
    )
    creds = claude_worker.objstore.Credentials(access_key_id="AK", secret_access_key="s")
    ticks = iter([1_000_000.0, 1_000_600.0, 1_000_600.0])
    store = claude_worker.objstore.ObjectStore(
        endpoint,
        creds,
        httpx.Client(transport=httpx.MockTransport(record)),
        timeout_s=5.0,
        clock=lambda: next(ticks),
        sleep=lambda _s: None,
    )
    fake.objects["v1/index/mbp-m4/run-1.json"] = b"{}"
    store.get_bytes("v1/index/mbp-m4/run-1.json")
    assert len(seen) == 2
    assert seen[0] != seen[1], "the retry reused the first signature"


def test_explicit_content_length_and_never_chunked(tmp_path: pathlib.Path) -> None:
    fake = tests.fake_s3.FakeS3()
    captured: list[httpx.Request] = []

    def record(request: httpx.Request) -> httpx.Response:
        captured.append(request)
        return fake.handle(request)

    endpoint = claude_worker.objstore.Endpoint(
        host="fsn1.your-objectstorage.com", region="fsn1", bucket=fake.bucket
    )
    store = claude_worker.objstore.ObjectStore(
        endpoint,
        claude_worker.objstore.Credentials(access_key_id="AK", secret_access_key="s"),
        httpx.Client(transport=httpx.MockTransport(record)),
        timeout_s=5.0,
        sleep=lambda _s: None,
    )
    src = tmp_path / "obj.bin"
    src.write_bytes(b"z" * 3000)
    store.put("v1/runs/mbp-m4/run-1/obj.bin", src)
    put = next(r for r in captured if r.method == "PUT")
    assert put.headers["content-length"] == "3000"
    assert "transfer-encoding" not in put.headers


def test_wire_query_is_byte_identical_to_the_signed_query() -> None:
    """httpx must not re-encode what we signed. Verified live over 37 probe
    requests; asserted here so an httpx upgrade cannot break it quietly."""
    fake = tests.fake_s3.FakeS3()
    captured: list[httpx.Request] = []

    def record(request: httpx.Request) -> httpx.Response:
        captured.append(request)
        return fake.handle(request)

    endpoint = claude_worker.objstore.Endpoint(
        host="fsn1.your-objectstorage.com", region="fsn1", bucket=fake.bucket
    )
    store = claude_worker.objstore.ObjectStore(
        endpoint,
        claude_worker.objstore.Credentials(access_key_id="AK", secret_access_key="s"),
        httpx.Client(transport=httpx.MockTransport(record)),
        timeout_s=5.0,
        sleep=lambda _s: None,
    )
    list(store.list("v1/runs/mbp-m4/", page_size=1000))
    sent = captured[0].url.query.decode("ascii")
    assert sent == "list-type=2&max-keys=1000&prefix=v1%2Fruns%2Fmbp-m4%2F"


def test_path_style_addressing_builds_the_bucket_into_the_path() -> None:
    fake = tests.fake_s3.FakeS3()
    store = make_store(fake, addressing="path")
    store.put_bytes("v1/index/mbp-m4/run-1.json", b"{}")
    assert fake.requests[0][1] == "/market-data-qu/v1/index/mbp-m4/run-1.json"
    assert store.get_bytes("v1/index/mbp-m4/run-1.json") == b"{}"


def test_key_with_unencodable_characters_is_refused() -> None:
    fake = tests.fake_s3.FakeS3()
    store = make_store(fake)
    with pytest.raises(claude_worker.objstore.ObjStoreError, match="escaping"):
        store.head("v1/runs/mbp-m4/run 1/_manifest.json")
    assert fake.count() == 0


def test_get_versioning_reads_status() -> None:
    fake = tests.fake_s3.FakeS3(versioning="Enabled")
    assert make_store(fake).get_versioning() == "Enabled"
    assert make_store(tests.fake_s3.FakeS3()).get_versioning() is None


def test_secret_never_appears_in_repr_or_exception_text() -> None:
    secret = "SENTINEL-SECRET-VALUE-do-not-log"
    creds = claude_worker.objstore.Credentials(access_key_id="AK", secret_access_key=secret)
    assert secret not in repr(creds)
    assert secret not in str(creds)

    fake = tests.fake_s3.FakeS3()
    fake.status_once["HEAD"] = 403
    endpoint = claude_worker.objstore.Endpoint(
        host="fsn1.your-objectstorage.com", region="fsn1", bucket=fake.bucket
    )
    store = claude_worker.objstore.ObjectStore(
        endpoint, creds, fake.client(), timeout_s=5.0, sleep=lambda _s: None
    )
    with pytest.raises(claude_worker.objstore.ObjStoreError) as caught:
        store.head("v1/index/mbp-m4/run-1.json")
    assert secret not in str(caught.value)
    assert secret not in repr(caught.value)
