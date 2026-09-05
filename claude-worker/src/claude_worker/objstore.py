# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""Minimal S3-compatible object store client (S3 archive lane, phase S1).

Handwritten SigV4 over ``hmac`` + ``hashlib`` + ``httpx``, plus stdlib
``xml.etree.ElementTree`` and ``urllib.parse``. **No new dependency** (S-LAW
10): no boto3, no botocore, no cloud SDK of any kind. The engine neither links
nor calls any of this (S-LAW 1) — it is offline Python that reads run
directories and talks to a cold archive.

Deliberate omissions, and why:

- **No ``delete_object``.** The archive is append-only and permanent (S-LAW
  11). The only ``DELETE``-verb request this client can send is
  ``abort_multipart``, which can affect nothing but the parts of an upload that
  never completed. Deleting a completed object is an operator act, by hand, in
  the provider console.
- **No ``presign``.** A presigned URL is a credential in a query string
  (S-LAW 6).
- **No ``put_lifecycle``.** Bucket lifecycle is the one lever that could make
  "forever" quietly end; it stays outside the shipped client.

Behaviour pinned by the S0 probe against the real endpoint (2026-09-05), not
assumed from AWS documentation:

- SigV4 only; a SigV2-style header is rejected 403.
- The credential-scope region is **not validated** by the endpoint — a wrong
  region fails silently, so it is configuration, never something an error will
  catch for us.
- ``x-amz-content-sha256`` carrying the real payload digest **is** verified
  server-side (a wrong digest returns 400 ``XAmzContentSHA256Mismatch``). That
  makes integrity free on every single request, so this client always signs the
  real digest and never uses ``UNSIGNED-PAYLOAD``.
- ``x-amz-checksum-*`` is accepted on PUT but not returned by HEAD, so it is
  not round-trippable here and is not used.
- ListObjectsV2 XML **is** namespaced. ``Element.iter()`` does NOT honour the
  ``{*}`` wildcard — only ``find``/``findall``/``iterfind`` do — so every tag
  lookup below uses ``findall``/``find``. Using ``iter`` silently returns an
  empty listing.
- Single-PUT ETag is the body md5; a completed multipart ETag is
  ``md5(concat(binary part md5s))-N``. Both are verified locally.
- The minimum non-final part size is 5 MiB (a smaller one fails the COMPLETE
  with ``EntityTooSmall``).

Convention: full ``import x`` only. No ``from x import y``.
"""

import builtins
import collections.abc
import dataclasses
import hashlib
import hmac
import http
import pathlib
import random
import re
import time
import typing
import urllib.parse
import xml.etree.ElementTree

import httpx

# Keys this archive generates are run ids, venue file names and manifest names.
# Restricting them to this set means the canonical URI never needs
# percent-encoding, which removes the single richest source of SigV4 bugs.
KEY_RE: typing.Final[re.Pattern[str]] = re.compile(r"^[A-Za-z0-9._/-]+$")

EMPTY_SHA256: typing.Final[str] = hashlib.sha256(b"").hexdigest()

#: Minimum size of a non-final multipart part, fixed by the S3 protocol.
MIN_PART_SIZE: typing.Final[int] = 5 * 1024 * 1024
#: Maximum number of parts in one multipart upload, fixed by the S3 protocol.
MAX_PARTS: typing.Final[int] = 10_000

#: Streaming chunk size for uploads and downloads.
CHUNK_SIZE: typing.Final[int] = 8 * 1024 * 1024

#: Headers that are never signed. ``content-length`` is excluded because httpx
#: may normalise it; the others are transport-managed.
UNSIGNED_HEADERS: typing.Final[frozenset[str]] = frozenset(
    {"authorization", "content-length", "user-agent", "accept-encoding", "accept", "connection"}
)

#: Retry only on these. Never on another 4xx: a stuck upload must fail loudly so
#: that S-LAW 3 keeps the local copy rather than deleting against a bad archive.
RETRY_STATUS: typing.Final[frozenset[int]] = frozenset({429, 500, 502, 503, 504})
MAX_ATTEMPTS: typing.Final[int] = 6
BACKOFF_CAP_S: typing.Final[float] = 32.0


class ObjStoreError(Exception):
    """An object-store request failed.

    The message carries the method, the key and the S3 error ``<Code>`` only.
    It never carries headers, a query string, or a credential (S-LAW 6): an
    exception string is a log line waiting to happen.
    """

    def __init__(self, message: str, *, status: int = 0, code: str = "") -> None:
        super().__init__(message)
        self.status = status
        self.code = code


class DigestMismatch(ObjStoreError):
    """The server's ETag disagreed with the digest computed locally."""


class BudgetSpent(ObjStoreError):
    """The wall budget ran out mid-upload. Resumable: nothing was committed."""


@dataclasses.dataclass(frozen=True, slots=True)
class Credentials:
    access_key_id: str
    # repr=False so the secret cannot reach a log, a traceback frame dump, or a
    # dataclass repr (config.py:40 precedent).
    secret_access_key: str = dataclasses.field(repr=False)


@dataclasses.dataclass(frozen=True, slots=True)
class Endpoint:
    host: str
    region: str
    bucket: str
    addressing: str = "virtual"

    def host_header(self) -> str:
        if self.addressing == "virtual":
            return f"{self.bucket}.{self.host}"
        return self.host

    def base_url(self) -> str:
        return "https://" + self.host_header()

    def path_for(self, key: str) -> str:
        """The request path exactly as it goes on the wire and into the signature."""
        if self.addressing == "virtual":
            return "/" + key if key else "/"
        return f"/{self.bucket}/{key}" if key else f"/{self.bucket}/"


class ObjectMeta(typing.NamedTuple):
    key: str
    size: int
    etag: str
    last_modified: str


@dataclasses.dataclass(slots=True)
class Budget:
    """A wall-clock budget shared across an upload.

    The archiver stops cleanly when it runs out rather than being killed by
    launchd mid-object: a half-written run with no manifest and no index is
    invisible to every reader and resumable tomorrow (S-LAW 5).
    """

    deadline: float
    clock: collections.abc.Callable[[], float] = time.monotonic

    def spent(self) -> bool:
        return self.clock() >= self.deadline


class SignedRequest(typing.NamedTuple):
    canonical_request: str
    string_to_sign: str
    signature: str
    authorization: str
    signed_headers: str


def percent(value: str) -> str:
    """RFC-3986 percent-encoding: unreserved ``A-Za-z0-9-_.~``; space is ``%20``."""
    return urllib.parse.quote(value, safe="-_.~")


def canonical_query(pairs: collections.abc.Sequence[tuple[str, str]]) -> str:
    """Encode, then sort by encoded name. A valueless subresource is ``name=``.

    The string returned here is BOTH signed and sent — building the URL from
    unencoded values afterwards is how signatures silently stop matching.
    """
    encoded = [(percent(name), percent(value)) for name, value in pairs]
    encoded.sort(key=lambda kv: (kv[0], kv[1]))
    return "&".join(name + "=" + value for name, value in encoded)


def sign(  # noqa: PLR0913 — one parameter per SigV4 input, deliberately
    *,
    method: str,
    path: str,
    query: str,
    headers: collections.abc.Mapping[str, str],
    payload_sha256_hex: str,
    amz_date: str,
    creds: Credentials,
    region: str,
    service: str = "s3",
) -> SignedRequest:
    """Compute the four SigV4 stages as a pure function.

    Pure on purpose: the golden-vector test exercises THIS, not the HTTP layer,
    so a signing regression names its own stage instead of surfacing as a 403
    from a live endpoint.
    """
    signed_names = sorted(
        name.lower() for name in headers if name.lower() not in UNSIGNED_HEADERS
    )
    lowered = {name.lower(): " ".join(str(value).split()) for name, value in headers.items()}
    canonical_headers = "".join(name + ":" + lowered[name] + "\n" for name in signed_names)
    signed_headers = ";".join(signed_names)
    canonical_request = (
        method
        + "\n"
        + path
        + "\n"
        + query
        + "\n"
        + canonical_headers
        + "\n"
        + signed_headers
        + "\n"
        + payload_sha256_hex
    )
    yyyymmdd = amz_date[:8]
    scope = yyyymmdd + "/" + region + "/" + service + "/aws4_request"
    string_to_sign = (
        "AWS4-HMAC-SHA256\n"
        + amz_date
        + "\n"
        + scope
        + "\n"
        + hashlib.sha256(canonical_request.encode("utf-8")).hexdigest()
    )
    key = ("AWS4" + creds.secret_access_key).encode("utf-8")
    for part in (yyyymmdd, region, service, "aws4_request"):
        key = hmac.new(key, part.encode("utf-8"), hashlib.sha256).digest()
    signature = hmac.new(key, string_to_sign.encode("utf-8"), hashlib.sha256).hexdigest()
    authorization = (
        "AWS4-HMAC-SHA256 Credential="
        + creds.access_key_id
        + "/"
        + scope
        + ", SignedHeaders="
        + signed_headers
        + ", Signature="
        + signature
    )
    return SignedRequest(
        canonical_request=canonical_request,
        string_to_sign=string_to_sign,
        signature=signature,
        authorization=authorization,
        signed_headers=signed_headers,
    )


def _error_code(body: bytes) -> str:
    """Pull ``<Code>`` out of an S3 error document; '' when it is not one."""
    try:
        root = xml.etree.ElementTree.fromstring(body)
    except xml.etree.ElementTree.ParseError:
        return ""
    node = root.find("{*}Code")
    if node is None:
        node = root.find("Code")
    if node is None or node.text is None:
        return ""
    return node.text


def _text(parent: xml.etree.ElementTree.Element, tag: str) -> str:
    node = parent.find(tag)
    if node is None or node.text is None:
        return ""
    return node.text


def _file_sha256_and_md5(path: pathlib.Path) -> tuple[str, str]:
    sha = hashlib.sha256()
    md5 = hashlib.md5()
    with path.open("rb") as handle:
        while True:
            chunk = handle.read(CHUNK_SIZE)
            if not chunk:
                break
            sha.update(chunk)
            md5.update(chunk)
    return sha.hexdigest(), md5.hexdigest()


class ObjectStore:
    """The verbs this archive needs, and no others."""

    def __init__(  # noqa: PLR0913 — one parameter per injected seam, deliberately
        self,
        endpoint: Endpoint,
        creds: Credentials,
        http_client: httpx.Client,
        *,
        timeout_s: float = 300.0,
        clock: collections.abc.Callable[[], float] = time.time,
        sleep: collections.abc.Callable[[float], None] = time.sleep,
        rng: random.Random | None = None,
    ) -> None:
        self.endpoint = endpoint
        self._creds = creds
        self._http = http_client
        self._timeout_s = timeout_s
        self._clock = clock
        self._sleep = sleep
        self._rng = rng if rng is not None else random.Random(0)

    # -- request plumbing ---------------------------------------------------

    def _amz_date(self) -> str:
        return time.strftime("%Y%m%dT%H%M%SZ", time.gmtime(self._clock()))

    def _build(  # noqa: PLR0913 — one parameter per request component, deliberately
        self,
        method: str,
        key: str,
        *,
        query: collections.abc.Sequence[tuple[str, str]],
        payload_sha256_hex: str,
        extra_headers: collections.abc.Mapping[str, str] | None,
        content: bytes | collections.abc.Iterable[bytes] | None,
        content_length: int | None,
    ) -> httpx.Request:
        path = self.endpoint.path_for(key)
        qs = canonical_query(query)
        url = self.endpoint.base_url() + path + ("?" + qs if qs else "")

        to_sign: dict[str, str] = {"host": self.endpoint.host_header()}
        if extra_headers:
            for name, value in extra_headers.items():
                to_sign[name.lower()] = value
        # Re-signed on EVERY attempt: x-amz-date must stay within 15 minutes of
        # server time, so a retry that reused the first signature would start
        # failing exactly when the network was worst.
        to_sign["x-amz-content-sha256"] = payload_sha256_hex
        to_sign["x-amz-date"] = self._amz_date()

        signed = sign(
            method=method,
            path=path,
            query=qs,
            headers=to_sign,
            payload_sha256_hex=payload_sha256_hex,
            amz_date=to_sign["x-amz-date"],
            creds=self._creds,
            region=self.endpoint.region,
        )
        wire = dict(to_sign)
        wire["authorization"] = signed.authorization
        if content_length is not None:
            # Explicit Content-Length: httpx honours it and drops its own
            # Transfer-Encoding: chunked. S3 rejects a chunked body without
            # aws-chunked signing, which this client deliberately does not
            # implement.
            wire["content-length"] = str(content_length)
        return self._http.build_request(
            method, url, headers=wire, content=content, timeout=self._timeout_s
        )

    def _send(  # noqa: PLR0913 — one parameter per request component, deliberately
        self,
        method: str,
        key: str,
        *,
        query: collections.abc.Sequence[tuple[str, str]] = (),
        payload_sha256_hex: str = EMPTY_SHA256,
        extra_headers: collections.abc.Mapping[str, str] | None = None,
        body: bytes | None = None,
        stream_path: pathlib.Path | None = None,
        stream_offset: int = 0,
        stream_length: int = 0,
        expect: collections.abc.Container[int] = (200, 204),
        allow: collections.abc.Container[int] = (),
        stream_response: bool = False,
    ) -> httpx.Response:
        """One request with bounded retries. Re-signs on every attempt."""
        if key and not KEY_RE.match(key):
            raise ObjStoreError(f"key is not representable without escaping: {key!r}")

        last: ObjStoreError | None = None
        for attempt in range(MAX_ATTEMPTS):
            content: bytes | collections.abc.Iterable[bytes] | None = None
            length: int | None = None
            if body is not None:
                content = body
                length = len(body)
            elif stream_path is not None:
                content = _file_chunks(stream_path, stream_offset, stream_length)
                length = stream_length

            request = self._build(
                method,
                key,
                query=query,
                payload_sha256_hex=payload_sha256_hex,
                extra_headers=extra_headers,
                content=content,
                content_length=length,
            )
            try:
                response = self._http.send(request, stream=stream_response)
            except httpx.TransportError as exc:
                last = ObjStoreError(
                    f"{method} {key or '/'}: transport error ({type(exc).__name__})"
                )
            else:
                if response.status_code in expect or response.status_code in allow:
                    return response
                payload = b"" if stream_response else response.content
                if stream_response:
                    response.close()
                code = _error_code(payload)
                last = ObjStoreError(
                    f"{method} {key or '/'}: HTTP {response.status_code}"
                    + (f" {code}" if code else ""),
                    status=response.status_code,
                    code=code,
                )
                if response.status_code not in RETRY_STATUS:
                    raise last
            if attempt + 1 < MAX_ATTEMPTS:
                delay = min(BACKOFF_CAP_S, 2.0**attempt)
                self._sleep(delay * (0.8 + 0.4 * self._rng.random()))
        raise last if last is not None else ObjStoreError(f"{method} {key or '/'}: no attempt made")

    # -- verbs --------------------------------------------------------------

    def head(self, key: str) -> ObjectMeta | None:
        """Metadata for ``key``; ``None`` when it does not exist."""
        response = self._send(
            "HEAD", key, expect=(200,), allow=(int(http.HTTPStatus.NOT_FOUND),)
        )
        if response.status_code == http.HTTPStatus.NOT_FOUND:
            return None
        return ObjectMeta(
            key=key,
            size=int(response.headers.get("content-length", "0")),
            etag=(response.headers.get("etag") or "").strip('"'),
            last_modified=response.headers.get("last-modified", ""),
        )

    def get(
        self, key: str, dst: pathlib.Path, *, expect_size: int | None = None
    ) -> ObjectMeta:
        """Stream ``key`` to ``dst``. Writes ``dst`` only on a complete read."""
        partial = dst.with_name(dst.name + ".part")
        partial.parent.mkdir(parents=True, exist_ok=True)
        response = self._send("GET", key, expect=(200,), stream_response=True)
        written = 0
        digest = hashlib.sha256()
        try:
            with partial.open("wb") as handle:
                for chunk in _response_chunks(response):
                    handle.write(chunk)
                    digest.update(chunk)
                    written += len(chunk)
            etag = (response.headers.get("etag") or "").strip('"')
            last_modified = response.headers.get("last-modified", "")
        finally:
            response.close()
        if expect_size is not None and written != expect_size:
            partial.unlink(missing_ok=True)
            raise ObjStoreError(
                f"GET {key}: truncated — expected {expect_size} bytes, read {written}"
            )
        partial.replace(dst)
        return ObjectMeta(key=key, size=written, etag=etag, last_modified=last_modified)

    def get_bytes(self, key: str, *, max_bytes: int = 4 << 20) -> bytes:
        """Small objects only — manifests and index entries."""
        response = self._send("GET", key, expect=(200,))
        payload = response.content
        if len(payload) > max_bytes:
            raise ObjStoreError(f"GET {key}: {len(payload)} bytes exceeds the {max_bytes} cap")
        return payload

    def put_bytes(
        self, key: str, body: bytes, *, content_type: str = "application/json"
    ) -> ObjectMeta:
        digest = hashlib.sha256(body).hexdigest()
        response = self._send(
            "PUT",
            key,
            payload_sha256_hex=digest,
            body=body,
            extra_headers={"content-type": content_type},
            expect=(200,),
        )
        etag = (response.headers.get("etag") or "").strip('"')
        expected = hashlib.md5(body).hexdigest()
        if etag and etag != expected:
            raise DigestMismatch(f"PUT {key}: ETag {etag} != md5 {expected}")
        return ObjectMeta(key=key, size=len(body), etag=etag, last_modified="")

    def put(
        self, key: str, src: pathlib.Path, *, sha256_hex: str = "", md5_hex: str = ""
    ) -> ObjectMeta:
        """Single-request PUT of a whole file, below the multipart threshold."""
        if not sha256_hex or not md5_hex:
            sha256_hex, md5_hex = _file_sha256_and_md5(src)
        size = src.stat().st_size
        response = self._send(
            "PUT",
            key,
            payload_sha256_hex=sha256_hex,
            stream_path=src,
            stream_offset=0,
            stream_length=size,
            expect=(200,),
        )
        etag = (response.headers.get("etag") or "").strip('"')
        if etag and etag != md5_hex:
            raise DigestMismatch(f"PUT {key}: ETag {etag} != md5 {md5_hex}")
        return ObjectMeta(key=key, size=size, etag=etag, last_modified="")

    def list(
        self,
        prefix: str,
        *,
        delimiter: str | None = None,
        start_after: str | None = None,
        page_size: int = 1000,
    ) -> collections.abc.Iterator[ObjectMeta]:
        """Every object under ``prefix``, following continuation tokens."""
        token: str | None = None
        while True:
            query: builtins.list[tuple[str, str]] = [
                ("list-type", "2"),
                ("prefix", prefix),
                ("max-keys", str(page_size)),
            ]
            if delimiter:
                query.append(("delimiter", delimiter))
            if start_after:
                query.append(("start-after", start_after))
            if token:
                query.append(("continuation-token", token))
            response = self._send("GET", "", query=query, expect=(200,))
            root = xml.etree.ElementTree.fromstring(response.content)
            # findall, never iter: iter() ignores the {*} wildcard and would
            # return an empty listing against namespaced XML (S0 finding).
            for node in root.findall("{*}Contents"):
                size_text = _text(node, "{*}Size")
                yield ObjectMeta(
                    key=_text(node, "{*}Key"),
                    size=int(size_text) if size_text else 0,
                    etag=_text(node, "{*}ETag").strip('"'),
                    last_modified=_text(node, "{*}LastModified"),
                )
            if _text(root, "{*}IsTruncated").lower() != "true":
                return
            token = _text(root, "{*}NextContinuationToken")
            if not token:
                return

    # `builtins.list` throughout this class: the method named `list` below
    # shadows the builtin for annotation resolution inside the class body.
    def list_prefixes(self, prefix: str, delimiter: str = "/") -> builtins.list[str]:
        query: builtins.list[tuple[str, str]] = [
            ("list-type", "2"),
            ("prefix", prefix),
            ("delimiter", delimiter),
            ("max-keys", "1000"),
        ]
        out: builtins.list[str] = []
        token: str | None = None
        while True:
            page = builtins.list(query)
            if token:
                page.append(("continuation-token", token))
            response = self._send("GET", "", query=page, expect=(200,))
            root = xml.etree.ElementTree.fromstring(response.content)
            for node in root.findall("{*}CommonPrefixes"):
                value = _text(node, "{*}Prefix")
                if value:
                    out.append(value)
            if _text(root, "{*}IsTruncated").lower() != "true":
                return out
            token = _text(root, "{*}NextContinuationToken")
            if not token:
                return out

    def multipart_put(
        self,
        key: str,
        src: pathlib.Path,
        *,
        part_size: int,
        budget: Budget | None = None,
    ) -> ObjectMeta:
        """Upload ``src`` in parts, verifying the composite ETag locally.

        Any failure after initiate aborts the upload (best effort) and raises,
        so the incomplete parts cannot linger as a phantom object.
        """
        size = src.stat().st_size
        if part_size < MIN_PART_SIZE:
            raise ObjStoreError(f"part size {part_size} is below the {MIN_PART_SIZE} minimum")
        parts_total = (size + part_size - 1) // part_size
        if parts_total > MAX_PARTS:
            raise ObjStoreError(f"{parts_total} parts exceeds the {MAX_PARTS} limit")

        initiate = self._send("POST", key, query=[("uploads", "")], expect=(200,))
        root = xml.etree.ElementTree.fromstring(initiate.content)
        upload_id = _text(root, "{*}UploadId")
        if not upload_id:
            raise ObjStoreError(f"POST {key}?uploads: no UploadId in the response")

        etags: builtins.list[str] = []
        md5s: builtins.list[bytes] = []
        try:
            for number in range(1, parts_total + 1):
                offset = (number - 1) * part_size
                length = min(part_size, size - offset)
                sha, md5 = _slice_digests(src, offset, length)
                response = self._send(
                    "PUT",
                    key,
                    query=[("partNumber", str(number)), ("uploadId", upload_id)],
                    payload_sha256_hex=sha,
                    stream_path=src,
                    stream_offset=offset,
                    stream_length=length,
                    expect=(200,),
                )
                etag = (response.headers.get("etag") or "").strip('"')
                if etag and etag != md5:
                    raise DigestMismatch(f"part {number} of {key}: ETag {etag} != md5 {md5}")
                etags.append(etag or md5)
                md5s.append(bytes.fromhex(md5))
                if budget is not None and budget.spent() and number < parts_total:
                    raise BudgetSpent(f"{key}: budget spent after part {number}/{parts_total}")

            body = (
                "<CompleteMultipartUpload>"
                + "".join(
                    f'<Part><PartNumber>{n}</PartNumber><ETag>"{e}"</ETag></Part>'
                    for n, e in enumerate(etags, start=1)
                )
                + "</CompleteMultipartUpload>"
            ).encode("utf-8")
            complete = self._send(
                "POST",
                key,
                query=[("uploadId", upload_id)],
                payload_sha256_hex=hashlib.sha256(body).hexdigest(),
                body=body,
                expect=(200,),
            )
        except BaseException:
            self.abort_multipart(key, upload_id)
            raise

        croot = xml.etree.ElementTree.fromstring(complete.content)
        server_etag = _text(croot, "{*}ETag").strip('"')
        composite = hashlib.md5(b"".join(md5s)).hexdigest() + "-" + str(parts_total)
        if server_etag and server_etag != composite:
            raise DigestMismatch(f"{key}: ETag {server_etag} != composite {composite}")
        return ObjectMeta(key=key, size=size, etag=server_etag or composite, last_modified="")

    def abort_multipart(self, key: str, upload_id: str) -> None:
        """The ONLY DELETE-verb request this client can send (S-LAW 11).

        It can affect nothing but the parts of an upload that never completed.
        Best effort: an abort that fails must not mask the error that caused it.
        """
        try:
            self._send(
                "DELETE",
                key,
                query=[("uploadId", upload_id)],
                expect=(204, 200),
                allow=(int(http.HTTPStatus.NOT_FOUND),),
            )
        except ObjStoreError:
            return

    def get_versioning(self) -> str | None:
        """``"Enabled"`` / ``"Suspended"`` / ``None`` when never configured."""
        response = self._send("GET", "", query=[("versioning", "")], expect=(200,))
        root = xml.etree.ElementTree.fromstring(response.content)
        status = _text(root, "{*}Status")
        return status or None


def _response_chunks(response: httpx.Response) -> collections.abc.Iterator[bytes]:
    """Body bytes in CHUNK_SIZE pieces.

    ``iter_raw`` is the real path: it skips the decode layer, so a download
    costs one copy per chunk instead of buffering the whole object. A transport
    that has already materialised the body — ``httpx.MockTransport`` does, which
    is why the test suite needs this branch — reports the stream as consumed;
    slice the buffer it already holds rather than pretending to stream it.
    """
    if response.is_stream_consumed:
        payload = response.content
        for offset in range(0, len(payload), CHUNK_SIZE):
            yield payload[offset : offset + CHUNK_SIZE]
        return
    yield from response.iter_raw(CHUNK_SIZE)


def _file_chunks(
    path: pathlib.Path, offset: int, length: int
) -> collections.abc.Iterator[bytes]:
    """Yield ``length`` bytes of ``path`` from ``offset`` in CHUNK_SIZE pieces.

    Copy ledger: one copy per chunk inside httpx/h11 (``h11.Connection.send``
    joins its data list into a fresh ``bytes``) plus the TLS record copy in
    OpenSSL. Both are unavoidable without a handwritten HTTP/TLS client, which
    this lane deliberately refuses to own.
    """
    remaining = length
    with path.open("rb") as handle:
        handle.seek(offset)
        while remaining > 0:
            chunk = handle.read(min(CHUNK_SIZE, remaining))
            if not chunk:
                return
            remaining -= len(chunk)
            yield chunk


def _slice_digests(path: pathlib.Path, offset: int, length: int) -> tuple[str, str]:
    sha = hashlib.sha256()
    md5 = hashlib.md5()
    for chunk in _file_chunks(path, offset, length):
        sha.update(chunk)
        md5.update(chunk)
    return sha.hexdigest(), md5.hexdigest()
