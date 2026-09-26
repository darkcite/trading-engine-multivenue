# rustls Zero-Copy Plan (TL0–TL7) — TLS records opened in place, no allocation per record

**Status: PLAN v1, 2026-09-26 — nothing built; eight operator questions open (§0).**
Authored on the operator's request (2026-09-26, "do the plan for rustls"): the one zero-copy item
the ZC pass left out of scope — core-net's rustls RX copy and rustls' allocation per record
(`CLAUDE.md` "Open: core-net's rustls RX copy …"; `docs/risk-policy.md` "ZC pass B" and the HYPARB
H9 residue).

**What this plan finds first.** The move the records point at — "switch core-net to rustls'
unbuffered API (`UnbufferedClientConnection`)" — removes nothing on rustls 0.23. Its read path copies
every application-data record into a fresh `Vec` exactly as the buffered path does, and its write path
allocates the same `Vec` and then copies it once more into the caller's buffer (§1.3, read in the
pinned 0.23.38 and the latest 0.23.45). rustls 0.24 decrypts in place, but no published release
exposes that yet (§1.4).

**What works on the pinned 0.23.38 today** is a small core-net record layer. rustls keeps the
handshake, certificate verification, the key schedule, key updates and session tickets, and hands the
traffic secrets over through its kernel-connection API (`dangerous_into_kernel_connection`, the kTLS
hook). core-net then opens each record in place in the driver's own rx with ring's `open_within`,
which decrypts and closes the gap between records in one pass (§1.6). The result is one kernel copy,
no allocation, and contiguous plaintext for the parsers that already exist.

**Where the work happens:** `crates/core-net` (`iobuf.rs`, `transport.rs`, a new `tls_record.rs`,
`drain.rs::fill_rx`). The nine ingress run loops change nothing. The exec-lane clients (`HlHttp`,
`UserWs`, `HttpsPost`, `LiveDispatcher`) move after the ingress soak (TL5b).
**Precedents:** D1's single read half (`core_net::fill_rx`), D3's header-first render (count, header,
render in place), BX0, ZC passes A and B.
**Opens nothing:**
- no new crate — ring 0.17.14 and rustls 0.23.38 are already in `Cargo.lock`; core-net only starts
  naming `ring` directly (Q8);
- no async;
- no `unsafe` in our code.

---

## §0 Operator rulings

All open. The recommendation is the plan's default if a question is not ruled.

| # | Question | Recommendation |
|---|---|---|
| Q1 | Path: our record layer on 0.23.38 now (Option B, §3), wait for rustls 0.24 (C), or B now and re-judge at 0.24.0? | **B now; re-judge at 0.24.0** (§3, §9 R4) |
| Q2 | TLS 1.2 connections (TL0 counts them): implement TLS 1.2 AEAD records (TL3b), or leave those connections on rustls? | **Leave them on rustls** unless TL0 shows a hot venue on TLS 1.2 |
| Q3 | Rollout switch: one global flag, or per venue in `universe.toml`? | **Per venue, default `rustls`**, flipped one venue at a time (TL5) |
| Q4 | Post-handshake NewSessionTicket: hand to rustls (resumption on reconnect) or drop? | **Hand to rustls** (`KernelConnection::handle_new_session_ticket`; cold, keeps reconnect handshakes short) |
| Q5 | Exec lane (`HlHttp`, `UserWs`, `HttpsPost`, `LiveDispatcher`) in the same pass, or after the ingress soak? | **After the soak** (TL5b) — the order path moves last |
| Q6 | TX render-into-record for the order path (TL6) now, or after TL5b measures it? | **After TL5b** |
| Q7 | Our TX direction at the AES-GCM confidentiality limit (rustls: `1 << 24` records for TLS 1.3 AES-GCM): send a KeyUpdate, or reconnect? | **KeyUpdate at 2^23 records** — far above any engine connection's volume; the check costs a compare |
| Q8 | core-net names `ring` as a direct dependency (already in `Cargo.lock` at 0.17.14 through rustls; the workspace pins it beside rustls), or reaches the AEAD through rustls' `MessageDecrypter` trait objects instead (in place but unshifted, and a `dyn` call per record)? | **Direct `ring`** — the shifted open is the point (§1.6) |

---

## §1 Facts — VERIFIED (source read 2026-09-26)

Paths below are in the cargo registry unless they start with `crates/` or `docs/`. rustls is pinned
at **0.23.38** (`Cargo.lock:1674`; workspace `Cargo.toml:119`: `default-features = false, features =
["ring", "std", "tls12"]`), ring at **0.17.14** (`Cargo.lock:1622`).

### 1.1 Where TLS runs in the engine (at `8bd7628`)

- **The transport.** `core_net::TlsTransport` (`crates/core-net/src/transport.rs:116-120`) is
  `{ sock: mio TcpStream, conn: rustls::ClientConnection, tcp_connected }`. It is 1 080 B and owns no
  buffer of its own; rustls owns all TLS buffering.
  - The `Transport` trait (`:65-108`): `interest`, `register`, `reregister`, `pump`, `read(dst)`,
    `write(src)`, `flush`.
  - The handshake and pump (`drive_tls`, `:193-258`): `read_tls` → `process_new_packets` → `write_tls`.
  - The read path (`:289-358`): `reader().read(dst)`, with a pull-through `read_tls` loop.
  - Writes (`:360-386`): `writer().write(src)` queues into rustls; `write_tls` sends from `drive_tls`
    or `flush`.
  - The module doc says "Steady-state `read` / `write` calls allocate 0 bytes" (`:14-16`, `:62-63`).
    Gate 72 shows that is false (below).
- **How bytes reach the parsers.** Every ingress `drive_one` reads through `core_net::fill_rx`
  (`crates/core-net/src/drain.rs:102-120`), which calls `transport.read(rx.free_mut())` and
  `rx.advance(n)`. The parsers read `rx.filled()` in place and `consume` (`crates/core-net/src/iobuf.rs:23-105`;
  compaction is `copy_within`, marked at `:69-73`).
- **Who uses the transport:**

  | Consumer | Where | Path |
  |---|---|---|
  | Nine TLS ingress crates (Polymarket, Binance, Bybit, MEXC, OKX, Deribit, Hyperliquid, RPC, HyperEVM) | `crates/cli/src/paper.rs:7872-7878` `connect_tls`; `fill_rx` in each `drive_one` | hot — market-data RX |
  | `HlHttp` `/exchange` + `/info` | `crates/exec-hyperliquid/src/http.rs:204-594` | hot — the order path; head and body go out as **two** records |
  | `UserWs` | `crates/exec-hyperliquid/src/userws_conn.rs:131-637` | hot — the fills of record |
  | `HttpsPost` | `crates/core-net/src/https_post.rs:192-637` | EVM arm, HYPARB reads, operator verbs |
  | `LiveDispatcher` | `crates/clob-dispatcher/src/live.rs:83-547` | Polymarket CLOB `/order` |
  | `boot_http` | `crates/core-net/src/boot_http.rs:176-177` (`rustls::Stream`) | cold — boot REST; **out of scope** |

- **The buffers the plaintext lands in.**
  - Ingress rx: 64 KiB (Polymarket, RPC, Binance spot) up to 4 MiB (OKX, Deribit), with Binance
    options at 2 MiB (`RX_BUF_SIZE` in each `run_loop.rs`).
  - `UserWs` rx: 1 MiB.
  - `HlHttp` response buffer: 1 MiB.
- **The measured residue.** Bench gate 72 (`crates/bench/tests/alloc_assertions.rs:8766-8858`) pins
  `HttpsPost` at exactly **2 allocations per request** — "one `Vec` per TLS record it seals … and one
  per application-data record it decrypts". Its comment, `docs/risk-policy.md:2605-2622` and
  `:5986-5993`, and `CLAUDE.md:153-155` all name the unbuffered API as the fix. §1.3 shows it is not.
  The ingress alloc gates drive `TestTransport`, so rustls' per-record allocation on ingress RX is
  unmeasured today; it is the same code path.

### 1.2 What rustls 0.23 does per record today (buffered)

- **RX:**
  1. `read_tls` copies ciphertext from the kernel into rustls' deframer buffer.
  2. AEAD decrypts in place there.
  3. The client state machine hands the application data to `take_received_plaintext`
     (`client/tls13.rs:1579`, `client/tls12.rs:1320`). That function appends `bytes.into_vec()`
     (`common_state.rs:483-487`), and `Payload::Borrowed(..).into_vec()` is `bytes.to_vec()`
     (`msgs/base.rs:41-46`): **an allocation and a copy per record**.
  4. `reader().read(dst)` copies again into our rx.

  **Per record: three copies (kernel, `Vec`, rx) and one allocation.** This is the "RX = 4 against the
  target of 3" of `docs/risk-policy.md:2591-2593`, the fourth being the POD into the ring.
- **TX:** `writer().write` → `send_appdata_encrypt` → `send_single_fragment` → `encrypt_outgoing`.
  The ring encrypter builds a `PrefixedPayload(Vec<u8>)` per record (`msgs/message/outbound.rs:215-222`),
  seals it in place, and `queue_tls_message` appends it to `sendable_tls` (`common_state.rs:436-439`).
  `write_tls` then copies to the kernel. **Per record: one allocation and two copies (plaintext into
  the `Vec`, `Vec` into the kernel).**

### 1.3 The 0.23 unbuffered API does not change that — the premise corrected

- **Read.** `process_tls_records` decrypts in the caller's buffer, but then runs the same state machine.
  - It yields `ReadTraffic` whenever `received_plaintext` is non-empty (`conn/unbuffered.rs:42-68`).
  - `ReadTraffic::next_record` pops a `Vec<u8>` from `received_plaintext` (`:349-360`), filled by the
    same `take_received_plaintext`.
  - The struct keeps the caller's buffer only "for forwards compatibility; to support in-place
    decryption in the future" (`:328-336`).
- **Write.** `WriteTraffic::encrypt` (`:445-457`) → `write_plaintext` (`common_state.rs:236`) →
  `write_fragments` (`:625-647`) encrypts each fragment into the same per-record `Vec`
  (`encrypt_outgoing(m).encode()`), then `copy_from_slice`s it into the caller's buffer. That is **one
  more copy than buffered**.
- **Latest stable.** The same holds in **0.23.45**, the latest stable (2026-09-14): the
  `unbuffered.rs:330` comment and `common_state.rs:483-487` are unchanged.
- **So:** migrating to `UnbufferedClientConnection` alone gains nothing on RX and loses a copy on TX.
  The records that say otherwise are corrected in TL7.

### 1.4 rustls 0.24 — in place, not yet published as such

- **Releases so far:** `0.24.0-dev.0` (2026-01-28) and `0.24.0-dev.1` (2026-07-23); stable is 0.23.45.
- **What the rustls team says.** The rustls team's post of 2026-09-08 describes 0.24's external buffering:
  - "Incoming plaintext is decrypted in place and returned as a borrow into the input buffer,
    avoiding copies".
  - A caller-owned `TlsInputBuffer`, and output appended to a caller `Vec<u8>`.
  - A split send/receive mode.
  - Crypto providers moved into separate crates (`rustls-ring`, `rustls-aws-lc-rs`).
  - A 1.0 after 0.24 has baked. No dates are given.
- **What the published dev.1 does.** It decrypts in place internally: `CaptureAppData` keeps an
  `UnborrowedPayload` range "for in-place decryption" (`conn/receive.rs:484-543`), over a
  `TlsInputBuffer` (`:880-904`). But its public buffered path still does
  `received_plaintext.append(payload.into_vec())` (`conn/mod.rs:239-245`), a `Vec` per record. The
  borrowed-payload surface (`read_tls(input, tls) -> MessageHandler`, `next_payload()`, `write(plaintext,
  &mut Vec<u8>)`) is only in the main-branch docs (rustls.dev, 2026-09). **The API is still moving
  between dev releases.**
- **Even at release it stops short of what the parsers want.** An in-place record's plaintext sits
  between its own header and tag, so two records are two separate slices. The engine's frame walkers
  expect one contiguous rx, so each record would still be copied, or every walker taught segments
  (Option F, §3).

### 1.5 The kernel-connection hook — in 0.23.38 today

- **The entry point.** `UnbufferedClientConnection::dangerous_into_kernel_connection(self) ->
  Result<(ExtractedSecrets, KernelConnection<ClientConnectionData>), Error>` (`client/client_conn.rs:943-949`).
  - The `kernel` module is public and not feature-gated (`lib.rs:548`).
  - It requires a finished handshake, nothing waiting to be sent, and
    `ClientConfig::enable_secret_extraction = true` (`client/client_conn.rs:219-221`;
    `conn/kernel.rs:15-33`).
- **What it hands over.**
  - `ExtractedSecrets { tx: (u64, ConnectionTrafficSecrets), rx: (u64, ConnectionTrafficSecrets) }` —
    the sequence number and secrets per direction (`suites.rs:195-201`).
  - `ConnectionTrafficSecrets` is `Aes128Gcm | Aes256Gcm | Chacha20Poly1305 { key: AeadKey, iv: Iv }`
    (`suites.rs:218-242`).
  - For TLS 1.2 AES-GCM, the `iv` carries the salt and the explicit-nonce seed
    (`crypto/ring/tls12.rs:159-171`).
- **What `KernelConnection` does — "only two things"** (`conn/kernel.rs:1-13`):
  - **Key updates:** `update_tx_secret` / `update_rx_secret` compute the next secret. They are TLS 1.3
    only, and the sequence number restarts at 0 (`:115-150`).
  - **Tickets:** `handle_new_session_ticket` stores a TLS 1.3 ticket.
- **Everything else is the user's**, including tracking the AES-GCM confidentiality limit
  (`conn/kernel.rs:37-51`; rustls' own limit for TLS 1.3 AES-GCM is `1 << 24` records,
  `crypto/ring/tls13.rs:48,70`).

### 1.6 ring 0.17.14 opens and seals in place, with a shift

- **`open_within`.** `LessSafeKey::open_within(nonce, aad, in_out, ciphertext_and_tag: RangeFrom<usize>)`
  (`aead/less_safe_key.rs:95-116`) leaves the plaintext at the front of `in_out`. Its documentation's
  own example is "Split stream reassembled in place": three sealed packets become one contiguous
  plaintext with three calls (`aead/opening_key.rs:110-125`).
- **One pass on the M4.** On aarch64 with AES and PMULL, `open` goes through `open_whole_partial` with
  an `Overlapping` input and output. The assembly kernel `aes_gcm_dec_kernel(input, …, output, …)` reads
  ahead of where it writes (`aead/aes_gcm.rs:304-352`; `aead/aes_gcm/aarch64.rs:55-95`).
  **Decryption and the shift are one pass; there is no separate memmove.** ChaCha20-Poly1305's `open`
  takes the same `Overlapping` (`aead/chacha20_poly1305/mod.rs:85-107`).
- **Sealing.** `seal_in_place_separate_tag(nonce, aad, in_out) -> Tag` seals in place
  (`aead/less_safe_key.rs:137-160`).

---

## §2 Thesis

For a contiguous frame parser, the best TLS receive path is this:
1. The ciphertext lands once (the read(2) into the driver's rx).
2. Each record is opened in place, with the shift that closes the framing between records.
3. The plaintext is parsed where it lies.

rustls 0.23 cannot do this (§1.3), and 0.24 does half of it and is not released (§1.4). But 0.23.38 and
ring already hold every piece: rustls keeps everything that makes TLS hard (§1.5), and a small core-net
record layer does what the kernel does under kTLS — open and seal records — with ring (§1.6).

| Per application-data record | Today (rustls 0.23 buffered) | 0.23 unbuffered | 0.24 in place (at release) | **This plan (Option B)** |
|---|---|---|---|---|
| RX copies after the NIC | 3 (kernel → deframer, → `Vec`, → rx) | 3 | 2 (kernel, → rx), or 1 with segment-aware parsers | **1** (kernel → rx); the AEAD pass lands the plaintext contiguously |
| RX heap allocations | 1 | 1 | 0 | **0** |
| TX copies | 2 (→ `Vec`, → kernel) | 3 | 2 (UNVERIFIED) | **2** (→ record, → kernel); **1** with TL6 |
| TX heap allocations | 1 | 1 | 0 if the output `Vec` is pre-sized (UNVERIFIED) | **0** |

That is RX = kernel + ring slot = **2 copies**, under the pass-B target of 3 (`docs/risk-policy.md:5127`).

---

## §3 Options considered

| | Option | Verdict |
|---|---|---|
| A | Move to rustls 0.23's `UnbufferedClientConnection` | **Rejected**: same `Vec` per record on RX, one more copy on TX (§1.3) |
| **B** | **Our record layer over `KernelConnection`: rustls for the handshake and secrets, ring `open_within` / `seal_in_place` in core-net** | **Recommended**: zero allocations, one RX copy, available on the pinned version; the security surface is bounded (§7) |
| C | Wait for rustls 0.24 | **Deferred**: not released; the in-place surface is unpublished; still one copy per record for a contiguous parser; the crypto provider moves crates. Re-judge at 0.24.0 (Q1): if 0.24 exposes the borrow and the kernel hook survives, B's handshake driver moves to it and B's record layer stays |
| D | Linux kTLS (`setsockopt(TLS_RX/TLS_TX)` with the extracted secrets) | **Not on macOS**, where the engine runs. B's boundary is exactly kTLS's, so a Linux colo box later swaps `tls_record` for the kernel, with `KernelConnection` unchanged |
| E | Patch or fork rustls 0.23 for in-place decryption | **Rejected**: 0.24 already redesigns this; a fork is permanent maintenance of TLS state machines |
| F | Parse records as separate segments (no shift), with 0.24 or B | **Deferred**: every run loop's frame walk would have to become segment-aware, for nothing over B's shift |

---

## §4 Design

### 4.1 `IoBuf` gains an undecoded tail

```text
0          head         tail         raw_start           raw_end         cap
| consumed | plaintext  |  gap       | ciphertext pending |  free          |
            ^ filled()   ^ closed by  ^ a partial record    ^ free_mut()
                           the next     (or records not
                           open         yet opened)
```

- `filled()`, `filled_mut()` and `consume()` are unchanged, and expose plaintext only.
- `free_mut()` returns `[raw_end..cap)`.
- Compaction runs only when `raw_end` is pinned at `cap` with dead bytes in front, as today. It moves
  the plaintext to 0 and the pending ciphertext right behind it: two `copy_within`, COPY-marked, each
  at most the unread plaintext or one partial record.
- Plain transports (`TestTransport`, `PlainTcpTransport`) never open a raw region
  (`raw_start == raw_end == tail`), so every existing test keeps its meaning.
- **Headroom law (const-asserted per crate):** a TLS rx holds the largest frame its parser accepts
  plus one maximal record — 16 645 B for TLS 1.3 (`5 + 2^14 + 256`), 18 437 B for TLS 1.2. Every
  current rx (64 KiB–4 MiB, §1.1) already clears it. A violation fails the session (fail-fast); it
  never stalls. D1's "rx already full reports `Drained`" guard keeps its meaning.

### 4.2 `Transport::fill`

```rust
/// Read what the socket has into `rx`: plaintext lands at `rx.tail`, a partial TLS
/// record stays in the raw tail. Default: `read(rx.free_mut())` + `advance`.
fn fill(&mut self, rx: &mut IoBuf) -> io::Result<Filled>;   // Filled { Bytes(n) | Eof }
```

- `core_net::fill_rx` (D1) calls `fill` instead of `read`. The nine ingress `drive_one`s change
  nothing, and neither do D1's `RxFill` and step cap.
- `Bytes(0)` is legal: a read that brought only part of a record.
- `read(dst)` stays for the request/response clients, as a copy out of the transport's own window,
  until TL5b moves them onto an `IoBuf`.

### 4.3 The record layer (`core_net::tls_record`) — pure, no I/O, no allocation

The unit of work is **one record opened in place**:

```rust
/// Open the record at `buf[rec..]`, writing its plaintext at `buf[dst..]` (dst ≤ rec).
fn open(key: &LessSafeKey, iv: &[u8; 12], seq: u64, buf: &mut [u8], dst: usize, rec: usize)
    -> Result<Opened /* { inner: InnerType, plain_len, record_len } */, RecordErr>;
/// Seal `buf[5..5 + len]` + the inner type as one record, header and tag included.
fn seal(key: &LessSafeKey, iv: &[u8; 12], seq: u64, buf: &mut [u8], len: usize, inner: InnerType)
    -> Result<usize, RecordErr>;
```

**TLS 1.3 open (RFC 8446 §5.2):**
1. Parse the 5-byte header before anything touches it: the outer type must be 23 and the version
   `0x0303`; `17 ≤ length ≤ 2^14 + 256`. Otherwise the error is `record_overflow` or
   `unexpected_message`.
2. Keep the header as the AAD. The plaintext may overwrite it, so it is copied to a `[u8; 5]` first.
3. The nonce is `iv XOR (0^4 ‖ seq_be64)`.
4. Call `open_within(nonce, aad, &mut buf[dst..rec + 5 + length], (rec + 5 - dst)..)`.
5. The inner type is the last non-zero byte; the zeros after it are padding. A plaintext with no
   non-zero byte is `unexpected_message`, and more than 2^14 bytes of content once the padding is
   stripped is `record_overflow` (RFC 8446 §5.2, §5.4).

**TLS 1.3 seal:** header, plaintext, then the inner-type byte; `seal_in_place_separate_tag`; then the
tag. The AAD is the header with the final length.

**Inner types after the handshake:**

| Inner type | Handling |
|---|---|
| 23 application data | `tail += plain_len` |
| 21 alert | Exactly one 2-byte alert per record (RFC 8446 §5.1; rustls `msgs/alert.rs:23`). `close_notify` → `Eof`; any other → fail the session with the alert's code, `user_canceled` included (rustls tolerates it as a warning; it precedes a close anyway) |
| 22 handshake | Reassembled in a small fixed buffer (≤ 16 KiB, fail-fast above). RFC 8446 §5.1: a message may span records, but no other record type may arrive before it completes (`unexpected_message`, as rustls `conn.rs:1057-1063`), and a zero-length fragment is fatal |
| &nbsp;&nbsp;↳ NewSessionTicket | → `KernelConnection::handle_new_session_ticket` (Q4) |
| &nbsp;&nbsp;↳ KeyUpdate | It must end its record: a message never spans a key change (RFC 8446 §5.1; rustls `common_state.rs:297-306`). `update_rx_secret`, rebuild the rx key, seq 0. If `update_requested` and no reply of ours is still unsent, seal our own KeyUpdate(`update_not_requested`) **with the current tx key**, then `update_tx_secret`, rebuild the tx key, seq 0 (RFC 8446 §4.6.3). Any other request value is `illegal_parameter` (rustls `common_state.rs:698-713`) |
| &nbsp;&nbsp;↳ anything else | Fatal (no post-handshake auth: we present no certificate) |
| other (20 ChangeCipherSpec, unknown) | Fatal |

**Rules:**
- **Bounds.** The sequence numbers are `u64` per direction, starting from `ExtractedSecrets`. They
  fail-fast at `u64::MAX`, and our TX sends a KeyUpdate at 2^23 records (Q7).
- **Caps, as rustls sets them.** At most 32 consecutive empty records (`conn.rs:1065-1077`, `:1305`),
  and at most 32 KeyUpdates without application data between them (`common_state.rs:949-975`, after
  BoringSSL's `kMaxKeyUpdates`). The next one fails.
- **TLS 1.2 (TL3b, only if Q2 asks for it):**
  - AEAD suites only.
  - AES-GCM nonce = 4-byte salt ‖ 8-byte explicit nonce carried in the record; ChaCha20 nonce =
    `iv XOR seq`.
  - AAD = `seq ‖ type ‖ version ‖ plaintext length`.
  - No inner type, no key update.
- **Discipline:** no `unsafe`; every slice bound checked before use; `debug_assert!` on the
  invariants of §7.
- **Errors:** any record error is fatal to the session (fail-fast doctrine), and it reconnects. A fatal
  alert (`bad_record_mac`, `record_overflow`, `unexpected_message`, as RFC 8446 §5.2 and §6 ask) is
  sealed and sent best-effort before the close.

### 4.4 The transport: `TlsTransport` gains a record-layer mode

- **Handshake (cold).**
  - `UnbufferedClientConnection::new(config, name)` is driven through `EncodeTlsData` /
    `TransmitTlsData` / `BlockedHandshake`, with handshake in/out buffers the transport allocates at
    connect.
  - rustls allocates inside the handshake as it does today; that is connect-time, as recorded.
  - `ClientConfig` gets `enable_secret_extraction = true`, in a second `Arc` built next to
    `default_client_config()` so that rustls-mode sockets keep today's config.
- **The switch.**
  - By the first `WriteTraffic` state, rustls has processed every complete record in the handshake
    buffer: the loop in `unbuffered.rs:42-164` deframes until nothing complete is left before it yields
    `TransmitTlsData` or `WriteTraffic`.
  - Application data, if any arrived that early, surfaces first as `ReadTraffic`. It is drained into
    rx — copied once; cold, and in practice empty for WebSocket and HTTP.
  - Then `dangerous_into_kernel_connection()`.
  - Keys are built: `UnboundKey::new(alg, key)` → `LessSafeKey`; the suite comes from
    `ConnectionTrafficSecrets`.
  - The sequence numbers are carried over.
  - The leftover partial record is copied once into rx's raw tail (connect-time, ≤ one record, marked).
- **Fallback.** If the negotiated version or suite is outside the record layer's scope (TLS 1.2 before
  TL3b, or an unknown suite), the socket stays on rustls (today's buffered path), with one log line per
  connection naming the venue, version and suite.
- **Steady state.**
  - `fill(rx)`: `read(2)` into `rx.free_mut()`, which grows the raw tail. Then every complete record is
    opened at `dst = tail`, `rec = raw_start` (tail grows, raw_start advances). A partial record stays
    raw.
  - `write(src)` seals into the transport's own `tls_tx` (≤ 16 KiB per record: a copy plus the seal).
    `flush` sends.
  - A KeyUpdate reply is queued the same way from inside `fill`.
- **Unchanged:** `interest`, `pump` and `Status` semantics (READABLE, plus WRITABLE while `tls_tx`
  holds bytes).
- **Size.** The TLS state (two `LessSafeKey`s, the IVs, sequence numbers, the `KernelConnection`, the
  handshake reassembly buffer, `tls_tx`) lives in one `Box` allocated at connect. The transport that
  moves into a run-loop slot stays about the size of today's (COPY markers
  `crates/ingress-{binance,bybit,mexc}/src/run_loop.rs` re-measured in TL3).

### 4.5 TX render-into-record (TL6, after Q6)

```rust
/// Render straight into the payload of the next record; the transport writes
/// the header and the inner type, seals in place and queues it.
fn with_record(&mut self, render: impl Fn(&mut WsPayload<'_>) -> Result<(), WsWriteErr>)
    -> io::Result<()>;
```

- This is D3's render, one layer down: count, then write into the record span, then seal.
- For `HlHttp`, head and body become one record instead of two (§1.1), rendered once.
- TX drops to one copy (the kernel's). The render runs twice, which is acceptable at one order per call.

---

## §5 Workstreams

**TL0 — Measure and decide (one probe log line; nothing on the hot path)**
- **Per-venue handshake facts.** On `Ready`, log the negotiated version and suite once per connection
  (`protocol_version()`, `negotiated_cipher_suite()`). Run every venue's live smoke, or read
  the paper engine's log after its next restart. Output: a table in §14.
- **Offline bench** (criterion, not gated): ns per record for rustls' buffered read path vs
  `open_within` in place with a 22-byte shift, at 64 B, 512 B, 4 KiB and 16 KiB records, AES-128-GCM
  and ChaCha20, on the M4. Also check ring's shifted open against `open_in_place` plus `copy_within`
  (the fused pass of §1.6, measured rather than assumed).
- **Exit:** the operator rules Q1–Q8.

**TL1 — `IoBuf` raw tail + `Transport::fill` (behaviour-preserving)**
- `iobuf.rs`: `raw_start` / `raw_end`, the compaction of §4.1, and accessors for the transport
  (`raw_mut`, `extend_plain`, `raw_consume`).
- `transport.rs`: `fill` with the default body. `drain.rs`: `fill_rx` → `fill`.
- **Tests:** every existing `IoBuf` and drain test unchanged. New: a raw tail survives compaction
  byte-exact; plain transports never open one; the headroom-law const asserts per crate.
- **Exit:** clippy, nextest, alloc gates unchanged (0 B/op), copy-audit `new=0`.

**TL2 — `tls_record` (pure)**
- `open` / `seal` for TLS 1.3 (AES-128-GCM, AES-256-GCM, ChaCha20-Poly1305), the inner-type scan,
  alert decoding, and handshake-message reassembly.
- **Tests:**
  - Known-answer vectors from RFC 8448. Its 1-RTT trace gives the application traffic keys and IVs
    and the protected `application_data` and alert records.
  - Differential against rustls: a rustls `ServerConnection` seals, `tls_record::open` opens —
    thousands of random lengths, 0 to 2^14.
  - Every tampered byte (header, body, tag) is refused.
  - Records split at every byte offset across two reads.
  - 32 empty records accepted, the 33rd refused.
  - Oversize, undersize and wrong-type headers refused.
  - The RFC 8446 §5.1 framing rules: an alert that is not exactly 2 bytes, a zero-length handshake
    fragment, another record type in the middle of a handshake message, and a KeyUpdate that does not
    end its record are refused; so are the 33rd KeyUpdate without application data between them and a
    plaintext of zeros only.
  - Sequence and nonce discipline.
- **Fuzz:** `tls_record_stream`, arbitrary bytes and splits against a fixed key: never panics, never
  reads past a bound, never accepts a modified record.
- **Exit:** the vectors pass, the differential runs 10^6 records clean, and the fuzz target runs 1 h
  from a poisoned start without a finding.

**TL3 — `TlsTransport` record-layer mode**
- The handshake driver and switch (§4.4), `fill`, `write` / `flush`, KeyUpdate both ways, tickets (Q4),
  `close_notify` both ways, and the fallback.
- **Loopback tests** (the existing rustls-server harnesses, `crates/core-net/tests/tls_burst_loopback.rs`
  pattern):
  - A 256 KiB burst and a trickle.
  - The server calls `refresh_traffic_keys()` mid-stream, with and without `update_requested`, and
    our reply decrypts on the server.
  - Tickets arrive before and after the first application data.
  - The server's `close_notify` gives `Eof`; a TCP FIN without it gives `UnexpectedEof`.
  - A flipped ciphertext bit fails the session.
  - A TLS 1.2-only server takes the fallback.
- **Exit:** every loopback green in both modes, via a test-matrix switch.

**TL3b — TLS 1.2 AEAD records.** Only if Q2 asks for them. Same tests, TLS 1.2 server.

**TL4 — Gates**
- **A new alloc gate:** steady-state RX and TX over a real rustls loopback, in a child process like
  gate 72 — 10 000 records each way at 0 allocations, the handshake outside the window. It is numbered
  when it lands; 73–77 are already claimed by work on unmerged branches.
- **Gate 72:** with `HttpsPost` on the record layer (TL5b), the pin goes 2 → **0** and the comment is
  rewritten.
- **copy-audit:** `transport.rs`'s nine rustls markers are rewritten for the two remaining copies (the
  read(2) and the TX seal) plus the two connect-time moves. The baseline stays byte-identical.
- **Miri:** nothing new — no `unsafe` of ours.
- **Latency:** the existing `ingest_to_strategy` p50/p99 in a paper A/B, per venue.

**TL5 — Ingress rollout (per venue, Q3)**
- A per-venue `tls = "rustls" | "record"` key, `rustls` by default.
- Flip MEXC first (it has a live smoke and two classes), then Binance, then the rest one at a time.
  Each flip runs its 60 s smoke and 24 h in the paper engine.
- **Watch:** reconnects, `*_sub_drops`, `*_ring_drops_total`, parse errors, and `ingest_to_strategy`
  p99.
- A flip back is a config change and a restart, not a rebuild.

**TL5b — Exec lane (Q5)**
- `UserWs` moves onto an `IoBuf` and `fill`. `HlHttp`, `HttpsPost` and `LiveDispatcher` read into
  their response buffers through an `IoBuf` view.
- Gate 72 → 0.
- The `hl_exchange_tls_loopback`, `hl_lifecycle_loopback`, `hl_userws_loopback`,
  `https_post_loopback`, `arm_tls_loopback` and `live_dispatcher_loopback` suites run in both modes.

**TL6 — TX render-into-record (Q6)**
- `with_record` (§4.5). `HlHttp` renders head and body into one record.
- Measure the order-path p50/p99 from render to `write(2)`.

**TL7 — Records**
- `docs/risk-policy.md`: a section for this pass, plus corrections at `:2605-2622` and `:5986-5993`
  ("the unbuffered API" → this plan).
- `CLAUDE.md`: state bullet and gates.
- The gate 72 comment.
- `.claude/agents/zero-copy-auditor.md:70` ("rustls owns its buffers; no in-place API").
- `transport.rs`'s module doc (`:14-16`, `:62-63`).

---

## §6 Zero-copy and zero-allocation compliance

| Copy that remains | Bound | Why unavoidable | Rejected alternative |
|---|---|---|---|
| read(2): kernel → rx raw tail | one socket read | the syscall boundary | io_uring / DPDK / kTLS: outside the no-async doctrine and not on macOS (§3 D) |
| AEAD open with shift | one record | the decryption itself writes the plaintext; the shift is fused into it (§1.6) | none — this is the minimum |
| TX: plaintext → record, then seal | ≤ 16 KiB per record | the seal needs header room in front and tag room behind | TL6 renders into the record instead |
| TX: `tls_tx` → kernel | one `write(2)` | the syscall boundary | as RX |
| Compaction of plaintext and raw tail | ≤ unread bytes + one partial record | only when pinned at capacity, amortised to zero (today's rule) | a ring buffer splits frames at the wrap |
| Switch: leftover handshake bytes → rx | ≤ one record, once per connect | rustls' handshake buffer is not the driver's rx | driving the handshake inside rx (couples `pump` to the driver's buffer; not worth it for a connect-time copy) |

**Allocations:** zero per record, both directions. The connect-time allocations are the handshake,
the `KernelConnection` (a `Box<dyn KernelState>`), and the transport's boxed TLS state; all are
recorded. `KernelConnection`'s `dyn` is called on key updates and tickets only, never per record.

---

## §7 Security

- **Threat model:** an on-path attacker who can inject, modify, reorder, truncate or drop bytes, and
  who sends hostile record lengths.
- **What stays in rustls, unchanged:** certificate verification (webpki roots), the handshake
  transcript, the key schedule, KeyUpdate secret derivation, and ticket storage.
- **Invariants** (`debug_assert!` in code, and each one also a test):
  1. The plaintext is never exposed before the tag verifies (`tail` advances only on `Ok`; ring
     clobbers `in_out` on failure, but that range starts at `tail`, outside `filled()`, and the
     session dies).
  2. `nonce = iv XOR seq`, and `seq` strictly increments per record per direction.
  3. A key update resets `seq` to 0 with the new key, never the old one.
  4. The AAD is the header as received (RX) or as written (TX).
  5. A length is checked against the buffer before any slice.
  6. Each `fill` call does bounded work.
- **A nonce reuse would be catastrophic** (AES-GCM). The TX sequence number lives in exactly one place,
  is incremented before the seal returns, and the key and sequence are replaced together.
- **Secrets:** they are extracted into the transport's box and dropped on disconnect.
  - rustls' `AeadKey` zeroizes on drop. ring's `LessSafeKey` does not; its key schedule is ordinary
    memory, like rustls' own ring keys today.
  - `enable_secret_extraction` is set only on the record-layer config.
- **Before any live socket:**
  - an adversarial security review (a separate reviewer; findings recorded);
  - the RFC 8448 vectors, the differential run and the fuzz run green;
  - the operator's sign-off.

---

## §8 Testing and gates (summary)

- **Unit, pure (TL2):** vectors, differential, tamper, split, bounds.
- **Loopback (TL3, TL5b):** every existing rustls-server suite in both modes, plus the KeyUpdate,
  ticket and close cases.
- **Fuzz:** `tls_record_stream`, plus today's frame targets unchanged.
- **Gates:** the new TLS alloc gate (0 per record), gate 72 → 0 (TL5b), copy-audit `new=0` with the baseline
  unchanged, clippy, nextest, license, fuzz build, Miri unchanged.
- **Live:** per-venue 60 s smokes and a 24 h paper soak per flip (TL5).
- **Latency:** the paper `ingest_to_strategy` A/B (TL4), and the order path render → `write(2)` (TL6).

---

## §9 Risks

| # | Risk | Mitigation |
|---|---|---|
| R1 | A bug in our record layer is a security bug | §7: rustls keeps the hard parts; the layer is pure and small; vectors, differential and fuzz; a separate review; flipped per venue behind a switch |
| R2 | A venue negotiates TLS 1.2 or an unexpected suite | The fallback keeps it on rustls (§4.4); TL0 counts them; Q2 |
| R3 | Peer behaviour rustls tolerates and we don't (padding, empty records, ticket bursts, KeyUpdate storms) | The same caps as rustls: 32 empty records, and 32 KeyUpdates without application data between them (§4.3). A KeyUpdate costs one HKDF-Expand. The one deliberate divergence: `user_canceled` ends the session. Loopback cases for each |
| R4 | `dangerous_into_kernel_connection` changes in 0.24 | 0.24's dev.1 still ships `conn/kernel.rs`. The record layer does not depend on rustls types past the switch; Q1 re-judges at 0.24.0 |
| R5 | Headroom law violated by a future buffer change | Const asserts per crate (TL1); a violation fails fast, never stalls |
| R6 | A bad flip in production | Per-venue config, flip back without a rebuild (TL5); smokes and soak before each flip |

---

## §10 Non-goals

- The server side, QUIC and 0-RTT.
- TLS 1.2 renegotiation (rustls never supported it).
- Client certificates.
- `boot_http` (cold, `rustls::Stream`).
- io_uring, DPDK, async.
- Linux kTLS itself (§3 D: the boundary is kept so that it can be done later).
- Moving to rustls 0.24 (Q1 re-judges it at release).

---

## §11 Sequencing and effort

| Step | Depends on | Effort | Ships |
|---|---|---|---|
| TL0 | — | 0.5 d | numbers + rulings |
| TL1 | TL0 | 1 d | a behaviour-preserving checkpoint commit |
| TL2 | TL1 | 2–3 d | a pure module, dark |
| TL3 (+TL3b) | TL2 | 2–3 d (+1 d) | the transport mode, dark (the switch defaults to `rustls`) |
| TL4 | TL3 | 1–2 d | gates |
| TL5 | TL4 + security review | 1 d + 24 h soak per flip | per-venue flips (operator deploys) |
| TL5b | TL5 soak | 2 d | exec lane; gate 72 → 0 |
| TL6 | TL5b | 1–2 d | the order path in one record |
| TL7 | each step | 0.5 d | records |

Each step is its own `ZC:` checkpoint commit, with the full gates. Deploying stays the operator's step.

---

## §12 Open questions and UNVERIFIED items

- **Q1–Q8:** see §0.
- **UNVERIFIED:**
  - whether ring fuses the shift for ChaCha20 as it does for AES-GCM on aarch64 (TL0 measures both);
  - 0.24's TX allocation behaviour at release (§2 table);
  - which venues negotiate TLS 1.2 (TL0).

---

## §13 Sources

- **rustls 0.23.38** (the pinned version; read on the Mac at
  `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/rustls-0.23.38`):
  - `src/conn/unbuffered.rs:42-164, 328-360, 424-435, 445-457, 481, 504-515`
  - `src/common_state.rs:236, 297-306, 436-439, 483-487, 625-647, 698-713, 949-975`
  - `src/msgs/base.rs:41-46`
  - `src/msgs/message/outbound.rs:215-222`
  - `src/client/tls13.rs:1579`
  - `src/client/tls12.rs:1320`
  - `src/conn/kernel.rs:1-51, 115-150`
  - `src/client/client_conn.rs:219-221, 890, 943-949`
  - `src/suites.rs:195-201, 218-242`
  - `src/crypto/ring/tls12.rs:159-171`
  - `src/crypto/ring/tls13.rs:48, 70`
  - `src/lib.rs:548`
  - `src/conn.rs:1057-1077, 1305`
  - `src/msgs/alert.rs:23`
- **rustls 0.23.45** (latest stable, 2026-09-14): `src/conn/unbuffered.rs:330`,
  `src/common_state.rs:483-487`.
- **rustls 0.24.0-dev.1** (2026-07-23): `src/conn/receive.rs:484-543, 712, 833, 880-904`;
  `src/conn/mod.rs:41-81, 225-245`.
- **ring 0.17.14:**
  - `src/aead/less_safe_key.rs:47-160`
  - `src/aead/opening_key.rs:97-125`
  - `src/aead/aes_gcm.rs:304-380`
  - `src/aead/aes_gcm/aarch64.rs:55-95`
  - `src/aead/chacha20_poly1305/mod.rs:85-107`
- **Web:**
  - [rustls — A Decade of Rustls (2026-09-08)](https://rustls.dev/blog/2026-09-08-a-decade-of-rustls/)
  - [rustls main-branch docs](https://rustls.dev/docs/rustls/index.html) (`TlsInputBuffer`, `MessageHandler`,
    `ClientConnection`)
  - [crates.io rustls versions](https://crates.io/crates/rustls)
  - [rustls issue #2761](https://github.com/rustls/rustls/issues/2761) (unbuffered reader/writer split)
  - RFC 8446 §4.6.3, §5.1, §5.2, §5.5
  - RFC 8448
- **This repository (`8bd7628`):**
  - `crates/core-net/src/{transport.rs,iobuf.rs,drain.rs,https_post.rs,boot_http.rs}`
  - `crates/exec-hyperliquid/src/{http.rs,userws_conn.rs}`
  - `crates/clob-dispatcher/src/live.rs`
  - `crates/cli/src/paper.rs:7872-7878`
  - `crates/bench/tests/alloc_assertions.rs:8766-8858`
  - `docs/risk-policy.md:2591-2622, 5127, 5965-5993`
  - `CLAUDE.md:153-155`

---

## §14 Progress log

- 2026-09-26 — Plan v1 written. Facts read from source (§1). Nothing built. Rulings open (§0).
