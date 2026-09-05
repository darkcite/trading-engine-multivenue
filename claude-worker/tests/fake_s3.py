# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""An in-process S3 good enough to test the archive lane against.

`httpx.MockTransport` over a `dict[str, bytes]`. No socket is ever opened —
"no network in tests, ever" is enforced by construction here rather than by a
convention someone has to remember.

It implements only what this lane sends: PUT (whole object), GET (with Range),
HEAD, ListObjectsV2 (paginated, `delimiter`, `start-after`), multipart
(`?uploads`, `?partNumber`, `?uploadId`, abort) and `?versioning`. It reproduces
the behaviours the S0 probe MEASURED against the real endpoint, including the
ones that bite:

- ListObjectsV2 XML is **namespaced** (`http://s3.amazonaws.com/doc/2006-03-01/`),
  so a parser that uses `Element.iter("{*}Tag")` sees an empty listing here too,
  exactly as it would in production.
- Single-PUT ETag is the body md5; a completed multipart ETag is
  `md5(concat(binary part md5s))-N`.
- A non-final part below 5 MiB makes the COMPLETE fail `EntityTooSmall`.
- `x-amz-content-sha256` is verified: a wrong digest is 400
  `XAmzContentSHA256Mismatch`.

Fault hooks (`fail_after_n_puts`, `status_once`, `truncate_get`, `corrupt_etag`)
exist so the fail-closed paths can be tested, and `requests` counts everything
so a "does zero HTTP when disabled" assertion is possible at all.

Convention: full ``import x`` only. No ``from x import y``.
"""

import collections.abc
import dataclasses
import hashlib
import typing
import urllib.parse

import httpx

NS: typing.Final[str] = "http://s3.amazonaws.com/doc/2006-03-01/"


def _xml(body: str) -> bytes:
    return ('<?xml version="1.0" encoding="UTF-8"?>' + body).encode("utf-8")


def _error(code: str, message: str = "") -> bytes:
    return _xml(f"<Error><Code>{code}</Code><Message>{message or code}</Message></Error>")


@dataclasses.dataclass(slots=True)
class _Upload:
    key: str
    parts: dict[int, bytes] = dataclasses.field(default_factory=dict)


class FakeS3:
    """A bucket, in memory, with the behaviours S0 measured."""

    def __init__(self, *, bucket: str = "market-data-qu", versioning: str = "") -> None:
        self.bucket = bucket
        self.objects: dict[str, bytes] = {}
        self.uploads: dict[str, _Upload] = {}
        self.versioning = versioning
        self.requests: list[tuple[str, str, str]] = []  # (method, path, query)
        self.put_keys: list[str] = []
        # fault hooks
        self.fail_after_n_puts: int | None = None
        self.status_once: dict[str, int] = {}
        self.truncate_get: set[str] = set()
        self.corrupt_etag: set[str] = set()
        self.reject_digest: set[str] = set()
        self._upload_seq = 0

    # -- helpers ---------------------------------------------------------

    def transport(self) -> httpx.MockTransport:
        return httpx.MockTransport(self.handle)

    def client(self) -> httpx.Client:
        return httpx.Client(transport=self.transport())

    def key_of(self, request: httpx.Request) -> str:
        path = request.url.path
        if path.startswith(f"/{self.bucket}/"):
            return path[len(self.bucket) + 2 :]
        return path.lstrip("/")

    def count(self, method: str = "") -> int:
        if not method:
            return len(self.requests)
        return sum(1 for m, _, _ in self.requests if m == method)

    # -- the handler -----------------------------------------------------

    def handle(  # noqa: PLR0911, PLR0912 — one branch per S3 verb, deliberately flat
        self, request: httpx.Request
    ) -> httpx.Response:
        key = self.key_of(request)
        raw_query = request.url.query.decode("ascii")
        self.requests.append((request.method, request.url.path, raw_query))
        query = dict(urllib.parse.parse_qsl(raw_query, keep_blank_values=True))

        forced = self.status_once.pop(request.method, 0)
        if forced:
            return httpx.Response(forced, content=_error("SlowDown"))

        if request.method in ("PUT", "POST") and "x-amz-content-sha256" in request.headers:
            declared = request.headers["x-amz-content-sha256"]
            actual = hashlib.sha256(request.content).hexdigest()
            if declared != actual or key in self.reject_digest:
                return httpx.Response(400, content=_error("XAmzContentSHA256Mismatch"))

        if request.method == "GET" and "versioning" in query and not key:
            status = f"<Status>{self.versioning}</Status>" if self.versioning else ""
            body = f'<VersioningConfiguration xmlns="{NS}">{status}</VersioningConfiguration>'
            return httpx.Response(200, content=_xml(body))
        if request.method == "GET" and query.get("list-type") == "2":
            return self._list(query)
        if request.method == "POST" and "uploads" in query:
            return self._initiate(key)
        if request.method == "PUT" and "uploadId" in query:
            return self._upload_part(key, query, request.content)
        if request.method == "POST" and "uploadId" in query:
            return self._complete(key, query, request.content)
        if request.method == "DELETE" and "uploadId" in query:
            self.uploads.pop(query["uploadId"], None)
            return httpx.Response(204)
        if request.method == "PUT":
            return self._put(key, request.content)
        if request.method == "HEAD":
            return self._head(key)
        if request.method == "GET":
            return self._get(key, request.headers.get("range", ""))
        if request.method == "DELETE":
            self.objects.pop(key, None)
            return httpx.Response(204)
        return httpx.Response(405, content=_error("MethodNotAllowed"))

    # -- verbs -----------------------------------------------------------

    def _etag(self, key: str, body: bytes) -> str:
        digest = hashlib.md5(body).hexdigest()
        return "0" * 32 if key in self.corrupt_etag else digest

    def _put(self, key: str, body: bytes) -> httpx.Response:
        if self.fail_after_n_puts is not None and len(self.put_keys) >= self.fail_after_n_puts:
            raise httpx.ReadError("injected fault: connection reset mid-PUT")
        self.objects[key] = body
        self.put_keys.append(key)
        return httpx.Response(200, headers={"ETag": '"' + self._etag(key, body) + '"'})

    def _head(self, key: str) -> httpx.Response:
        if key not in self.objects:
            return httpx.Response(404, content=_error("NoSuchKey"))
        body = self.objects[key]
        return httpx.Response(
            200,
            headers={
                "Content-Length": str(len(body)),
                "ETag": '"' + self._etag(key, body) + '"',
                "Last-Modified": "Sat, 05 Sep 2026 12:00:00 GMT",
            },
        )

    def _get(self, key: str, range_header: str) -> httpx.Response:
        if key not in self.objects:
            return httpx.Response(404, content=_error("NoSuchKey"))
        body = self.objects[key]
        status = 200
        if range_header.startswith("bytes="):
            spec = range_header[len("bytes=") :]
            start_text, _, end_text = spec.partition("-")
            start = int(start_text) if start_text else 0
            end = int(end_text) if end_text else len(body) - 1
            body = body[start : end + 1]
            status = 206
        if key in self.truncate_get:
            body = body[: len(body) // 2]
        return httpx.Response(
            status,
            content=body,
            headers={
                "ETag": '"' + self._etag(key, self.objects[key]) + '"',
                "Last-Modified": "Sat, 05 Sep 2026 12:00:00 GMT",
            },
        )

    def _list(self, query: dict[str, str]) -> httpx.Response:
        prefix = query.get("prefix", "")
        delimiter = query.get("delimiter", "")
        start_after = query.get("start-after", "")
        token = query.get("continuation-token", "")
        max_keys = int(query.get("max-keys", "1000"))

        keys = sorted(k for k in self.objects if k.startswith(prefix))
        if start_after:
            keys = [k for k in keys if k > start_after]
        if token:
            keys = [k for k in keys if k > token]

        contents: list[str] = []
        prefixes: list[str] = []
        seen: set[str] = set()
        emitted = 0
        last = ""
        for key in keys:
            if delimiter:
                rest = key[len(prefix) :]
                if delimiter in rest:
                    common = prefix + rest.split(delimiter, 1)[0] + delimiter
                    if common in seen:
                        continue
                    seen.add(common)
                    prefixes.append(f"<CommonPrefixes><Prefix>{common}</Prefix></CommonPrefixes>")
                    emitted += 1
                    last = key
                    if emitted >= max_keys:
                        break
                    continue
            body = self.objects[key]
            contents.append(
                "<Contents>"
                f"<Key>{key}</Key><Size>{len(body)}</Size>"
                f'<ETag>&quot;{self._etag(key, body)}&quot;</ETag>'
                "<LastModified>2026-09-05T12:00:00.000Z</LastModified>"
                "</Contents>"
            )
            emitted += 1
            last = key
            if emitted >= max_keys:
                break
        truncated = emitted >= max_keys and emitted < len(keys)
        tail = f"<NextContinuationToken>{last}</NextContinuationToken>" if truncated else ""
        return httpx.Response(
            200,
            content=_xml(
                f'<ListBucketResult xmlns="{NS}">'
                f"<Name>{self.bucket}</Name><Prefix>{prefix}</Prefix>"
                f"<IsTruncated>{'true' if truncated else 'false'}</IsTruncated>"
                + "".join(contents)
                + "".join(prefixes)
                + tail
                + "</ListBucketResult>"
            ),
        )

    def _initiate(self, key: str) -> httpx.Response:
        self._upload_seq += 1
        upload_id = f"2~upload-{self._upload_seq}"
        self.uploads[upload_id] = _Upload(key=key)
        return httpx.Response(
            200,
            content=_xml(
                f'<InitiateMultipartUploadResult xmlns="{NS}">'
                f"<Key>{key}</Key><UploadId>{upload_id}</UploadId>"
                "</InitiateMultipartUploadResult>"
            ),
        )

    def _upload_part(self, key: str, query: dict[str, str], body: bytes) -> httpx.Response:
        upload = self.uploads.get(query.get("uploadId", ""))
        if upload is None:
            return httpx.Response(404, content=_error("NoSuchUpload"))
        if self.fail_after_n_puts is not None and len(self.put_keys) >= self.fail_after_n_puts:
            raise httpx.ReadError("injected fault: connection reset mid-part")
        number = int(query.get("partNumber", "0"))
        upload.parts[number] = body
        self.put_keys.append(f"{key}#part{number}")
        return httpx.Response(200, headers={"ETag": '"' + hashlib.md5(body).hexdigest() + '"'})

    def _complete(self, key: str, query: dict[str, str], body: bytes) -> httpx.Response:
        upload_id = query.get("uploadId", "")
        upload = self.uploads.get(upload_id)
        if upload is None:
            return httpx.Response(404, content=_error("NoSuchUpload"))
        numbers = sorted(upload.parts)
        for number in numbers[:-1]:
            if len(upload.parts[number]) < 5 * 1024 * 1024:
                return httpx.Response(400, content=_error("EntityTooSmall"))
        assembled = b"".join(upload.parts[n] for n in numbers)
        self.objects[key] = assembled
        del self.uploads[upload_id]
        joined = b"".join(hashlib.md5(upload.parts[n]).digest() for n in numbers)
        composite = hashlib.md5(joined).hexdigest() + "-" + str(len(numbers))
        if key in self.corrupt_etag:
            composite = "0" * 32 + "-" + str(len(numbers))
        return httpx.Response(
            200,
            content=_xml(
                f'<CompleteMultipartUploadResult xmlns="{NS}">'
                f"<Key>{key}</Key><ETag>&quot;{composite}&quot;</ETag>"
                "</CompleteMultipartUploadResult>"
            ),
        )


def make(**kwargs: typing.Any) -> tuple["FakeS3", httpx.Client]:
    """A fake bucket and a client wired to it."""
    fake = FakeS3(**kwargs)
    return fake, fake.client()


def collect(iterator: collections.abc.Iterable[typing.Any]) -> list[typing.Any]:
    return list(iterator)
