# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)

"""Generate Hyperliquid known-answer vectors from the OFFICIAL Python SDK.

LAW E-3 says msgpack key order is part of the signature, and that this
cannot be validated by reading the docs. So it is validated against the
SDK that the venue's own users sign with: this script drives
`hyperliquid.utils.signing` directly and freezes what it produces.

Run it with the SDK installed in a THROWAWAY venv. It must never be run
from `claude-worker`'s environment: the worker's dependency surface is a
frozen contract and the SDK is not part of it.

THE KEY BELOW IS A TEST KEY. It is a published, worthless, synthetic
value that exists so a signature is reproducible. It has never held
anything and must never be used for anything.
"""

import importlib.metadata
import json
import hyperliquid.utils.signing as signing
import eth_account
import eth_account.messages
import msgpack

TEST_KEY = "0x0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
VAULT = "0x1234567890abcdef1234567890abcdef12345678"

WALLET = eth_account.Account.from_key(TEST_KEY)


def limit(tif):
    return {"limit": {"tif": tif}}


def wire(asset, is_buy, px, sz, reduce_only, tif, cloid=None):
    """The SDK's OWN order wire — `order_request_to_order_wire` — never a
    hand-written mirror of its key order.

    LAW E-3 is about exactly that key order, and the first cut of this
    script wrote the dict by hand "mirroring" the SDK, which meant the
    fixture's authority stopped one function short of its header's
    claim (E7 review, 2026-09-19). The SDK builds it now; if the SDK
    ever changes the order, regeneration shows it as a diff.
    """
    req = {
        "coin": "",  # unused by the wire builder; the asset id is passed
        "is_buy": is_buy,
        "sz": sz,
        "limit_px": px,
        "order_type": limit(tif),
        "reduce_only": reduce_only,
    }
    if cloid is not None:
        req["cloid"] = _Cloid(cloid)
    return signing.order_request_to_order_wire(req, asset)


class _Cloid:
    """The SDK's `Cloid` duck: `to_raw()` returns the 0x-hex string."""

    def __init__(self, raw):
        self._raw = raw

    def to_raw(self):
        return self._raw


def order_action(wires, grouping="na"):
    # The SDK hardcodes `grouping = "na"`; the key is overwritten in
    # place, which keeps its position (dict insertion order), so the
    # msgpack key order stays the SDK's.
    action = signing.order_wires_to_order_action(wires)
    action["grouping"] = grouping
    return action


# HIP-4 asset ids: 100_000_000 + enc, enc = 10 * outcome_id + side.
HIP4_YES = 100_000_000 + 10 * 3253 + 0
HIP4_NO = 100_000_000 + 10 * 3253 + 1
CLOID_A = "0x00000000000000000000000000000001"
CLOID_B = "0x4d56030000000000000000000000002a"

CASES = []


def case(name, action, nonce, is_mainnet, vault=None, expires_after=None):
    CASES.append(
        {
            "name": name,
            "action": action,
            "nonce": nonce,
            "is_mainnet": is_mainnet,
            "vault": vault,
            "expires_after": expires_after,
        }
    )


# --- orders: the three tifs, both sides, with and without cloid -------
case("order_gtc_buy", order_action([wire(0, True, 0.5, 10.0, False, "Gtc")]), 1, True)
case("order_ioc_buy", order_action([wire(0, True, 0.5, 10.0, False, "Ioc")]), 2, True)
case("order_alo_buy", order_action([wire(0, True, 0.5, 10.0, False, "Alo")]), 3, True)
case("order_gtc_sell", order_action([wire(0, False, 0.5, 10.0, False, "Gtc")]), 4, True)
case("order_ioc_cloid", order_action([wire(0, True, 0.5, 10.0, False, "Ioc", CLOID_A)]), 5, True)
case("order_alo_cloid_testnet", order_action([wire(0, True, 0.5, 10.0, False, "Alo", CLOID_B)]), 6, False)
case("order_reduce_only", order_action([wire(0, False, 0.5, 10.0, True, "Gtc")]), 7, True)

# --- HIP-4 outcome markets: the asset ids and 4dp prices we trade -----
case("hip4_yes_ioc", order_action([wire(HIP4_YES, True, 0.4567, 25.0, False, "Ioc", CLOID_B)]), 8, True)
case("hip4_no_alo", order_action([wire(HIP4_NO, False, 0.9990, 1.0, False, "Alo")]), 9, True)
case("hip4_min_px", order_action([wire(HIP4_YES, True, 0.001, 10.0, False, "Ioc")]), 10, True)
case("hip4_max_px", order_action([wire(HIP4_YES, True, 0.999, 10.0, False, "Ioc")]), 11, True)
case("hip4_trailing_zero_px", order_action([wire(HIP4_YES, True, 0.5000, 100.0, False, "Gtc")]), 12, True)

# --- batches ----------------------------------------------------------
case(
    "order_batch_2",
    order_action([wire(HIP4_YES, True, 0.4, 5.0, False, "Ioc"), wire(HIP4_NO, True, 0.6, 5.0, False, "Ioc")]),
    13,
    True,
)
case(
    "order_batch_3_mixed",
    order_action(
        [
            wire(0, True, 0.5, 1.0, False, "Gtc", CLOID_A),
            wire(1, False, 1.5, 2.0, True, "Ioc"),
            wire(HIP4_YES, True, 0.25, 3.0, False, "Alo", CLOID_B),
        ]
    ),
    14,
    True,
)
case("order_grouping_tpsl", order_action([wire(0, True, 0.5, 10.0, False, "Gtc")], "normalTpsl"), 15, True)

# --- vault + expiresAfter: the two tail bytes of the action hash ------
case("order_vault", order_action([wire(0, True, 0.5, 10.0, False, "Gtc")]), 16, True, vault=VAULT)
case("order_expires", order_action([wire(0, True, 0.5, 10.0, False, "Gtc")]), 17, True, expires_after=1789000000000)
case(
    "order_vault_expires",
    order_action([wire(0, True, 0.5, 10.0, False, "Gtc")]),
    18,
    True,
    vault=VAULT,
    expires_after=1789000000000,
)

# --- cancels ----------------------------------------------------------
case("cancel_single", {"type": "cancel", "cancels": [{"a": 0, "o": 12345}]}, 19, True)
case(
    "cancel_batch_3",
    {"type": "cancel", "cancels": [{"a": 0, "o": 1}, {"a": 1, "o": 2}, {"a": HIP4_YES, "o": 99999999}]},
    20,
    True,
)
case("cancel_testnet", {"type": "cancel", "cancels": [{"a": 0, "o": 7}]}, 21, False)
case("cancel_by_cloid", {"type": "cancelByCloid", "cancels": [{"asset": HIP4_YES, "cloid": CLOID_B}]}, 22, True)
case(
    "cancel_by_cloid_batch_2",
    {"type": "cancelByCloid", "cancels": [{"asset": 0, "cloid": CLOID_A}, {"asset": HIP4_NO, "cloid": CLOID_B}]},
    23,
    True,
)

# --- modify -----------------------------------------------------------
case(
    "batch_modify_1",
    {"type": "batchModify", "modifies": [{"oid": 555, "order": wire(HIP4_YES, True, 0.48, 25.0, False, "Alo", CLOID_B)}]},
    24,
    True,
)
case(
    "batch_modify_2",
    {
        "type": "batchModify",
        "modifies": [
            {"oid": 1, "order": wire(0, True, 0.5, 1.0, False, "Gtc")},
            {"oid": CLOID_A, "order": wire(1, False, 2.5, 3.0, False, "Ioc", CLOID_A)},
        ],
    },
    25,
    True,
)

# --- request weight (S7-L1): the address-budget top-up ----------------
# The SDK has no helper for this action, so the dict IS the reference:
# the documented key order (`type`, `weight`; `destination` is skipped
# when unset), signed by the SDK's own L1 chain like every row above.
case("reserve_weight", {"type": "reserveRequestWeight", "weight": 5000}, 26, True)
case("reserve_weight_testnet", {"type": "reserveRequestWeight", "weight": 1}, 27, False)

out = []
for c in CASES:
    packed = msgpack.packb(c["action"])
    h = signing.action_hash(c["action"], c["vault"], c["nonce"], c["expires_after"])
    agent = signing.construct_phantom_agent(h, c["is_mainnet"])
    payload = signing.l1_payload(agent)
    encoded = eth_account.messages.encode_typed_data(full_message=payload)
    digest = eth_account.messages._hash_eip191_message(encoded)
    sig = WALLET.sign_message(encoded)
    out.append(
        {
            "name": c["name"],
            "action_json": json.dumps(c["action"], separators=(",", ":")),
            "nonce": c["nonce"],
            "vault": c["vault"],
            "expires_after": c["expires_after"],
            "source": "a" if c["is_mainnet"] else "b",
            "msgpack_hex": packed.hex(),
            "connection_id_hex": h.hex(),
            "eip712_digest_hex": digest.hex(),
            "sig_r_hex": f"{sig.r:064x}",
            "sig_s_hex": f"{sig.s:064x}",
            "sig_v": sig.v,
            "sig65_hex": f"{sig.r:064x}{sig.s:064x}{sig.v:02x}",
        }
    )

# TSV, not JSON: the Rust side reads this in a test and the repo does
# not carry a JSON parser outside the hot-path scanners. One row per
# vector, tab-separated, `#` comments. Same shape as the xsd/regime
# parity fixtures.
COLS = [
    "name",
    "source",
    "nonce",
    "vault",
    "expires_after",
    "msgpack_hex",
    "connection_id_hex",
    "eip712_digest_hex",
    "sig65_hex",
]
lines = [
    "# Hyperliquid L1-action known-answer vectors.",
    "# GENERATED by gen_vectors.py from the OFFICIAL hyperliquid-python-sdk.",
    "# Do not hand-edit: regenerate, and let the diff show what the SDK changed.",
    "#",
    "# LAW E-3: msgpack key order is part of the signature. These rows are the",
    "# only thing that validates it — the docs cannot.",
    "#",
    f"# hyperliquid-python-sdk: {importlib.metadata.version('hyperliquid-python-sdk')}",
    f"# signing key (TEST ONLY, worthless, published): {TEST_KEY}",
    f"# address: {WALLET.address}",
    f"# vault used where a vault is exercised: {VAULT}",
    "#",
    "\t".join(COLS),
]
for v in out:
    lines.append(
        "\t".join(
            [
                v["name"],
                v["source"],
                str(v["nonce"]),
                v["vault"] or "-",
                str(v["expires_after"]) if v["expires_after"] is not None else "-",
                v["msgpack_hex"],
                v["connection_id_hex"],
                v["eip712_digest_hex"],
                v["sig65_hex"],
            ]
        )
    )
print("\n".join(lines))
