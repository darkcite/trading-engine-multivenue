#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
#
# scripts/hyparb-executor-repro.sh — the O-H18 reproducibility check.
#
# The HYPARB executor is deployed from the COMMITTED bytecode
# (contracts/hyparb-executor/HyparbExecutor.bin), never from a local
# build. This script proves that bytecode is exactly what the committed
# source compiles to:
#
#   1. fetch solc 0.8.28 for this platform from binaries.soliditylang.org
#      into target/ (git-ignored) and verify its pinned sha256;
#   2. compile src/HyparbExecutor.sol from a standard-JSON input whose
#      settings are pinned HERE (optimizer 10000 runs, evm cancun, no
#      metadata hash, no CBOR tail — so the bytes depend on the source
#      and the settings alone, not on paths or the machine);
#   3. compare creation and runtime bytecode with the committed files.
#
#   scripts/hyparb-executor-repro.sh            # check (exit 1 on mismatch)
#   scripts/hyparb-executor-repro.sh --write    # regenerate the .bin files
#
# --write is for a deliberate source change only; the diff of the .bin
# files is then reviewed with the source diff.

set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
dir="$root/contracts/hyparb-executor"
solc_dir="$root/target/solc"
ver="0.8.28+commit.7893614a"

case "$(uname -s)" in
    Darwin)
        url="https://binaries.soliditylang.org/macosx-amd64/solc-macosx-amd64-v$ver"
        sha="81515b0e53deaa266d549545ccaac0a5a96e6d4e8201c77f673b2c710976d9ea"
        ;;
    Linux)
        url="https://binaries.soliditylang.org/linux-amd64/solc-linux-amd64-v$ver"
        sha="9a0fb7e0db2c0641dbae1c5cc645dc686820c83af516226abb1c0a2f76636f25"
        ;;
    *)
        echo "unsupported platform $(uname -s)" >&2
        exit 2
        ;;
esac

mkdir -p "$solc_dir"
solc="$solc_dir/solc-$ver"
if [ ! -x "$solc" ]; then
    curl -sSfL -o "$solc.part" "$url"
    mv "$solc.part" "$solc"
    chmod +x "$solc"
fi
got="$(shasum -a 256 "$solc" 2>/dev/null || sha256sum "$solc")"
got="${got%% *}"
if [ "$got" != "$sha" ]; then
    echo "solc sha256 mismatch: got $got want $sha" >&2
    exit 1
fi

out="$(python3 - "$dir/src/HyparbExecutor.sol" <<'PY' | "$solc" --standard-json
import json
import sys

src = open(sys.argv[1]).read()
print(json.dumps({
    "language": "Solidity",
    "sources": {"HyparbExecutor.sol": {"content": src}},
    "settings": {
        "optimizer": {"enabled": True, "runs": 10000},
        "evmVersion": "cancun",
        "metadata": {"bytecodeHash": "none", "appendCBOR": False},
        "outputSelection": {"HyparbExecutor.sol": {"HyparbExecutor": ["evm.bytecode.object", "evm.deployedBytecode.object"]}},
    },
}))
PY
)"

extract() {
    python3 -c '
import json
import sys

d = json.load(sys.stdin)
errs = [e for e in d.get("errors", []) if e.get("severity") == "error"]
if errs:
    sys.exit("solc: " + errs[0]["formattedMessage"])
c = d["contracts"]["HyparbExecutor.sol"]["HyparbExecutor"]["evm"]
print(c[sys.argv[1]]["object"])
' "$1"
}

creation="$(printf '%s' "$out" | extract bytecode)"
runtime="$(printf '%s' "$out" | extract deployedBytecode)"

if [ "${1:-}" = "--write" ]; then
    printf '%s\n' "$creation" > "$dir/HyparbExecutor.bin"
    printf '%s\n' "$runtime" > "$dir/HyparbExecutor.runtime.bin"
    echo "wrote HyparbExecutor.bin (${#creation} hex) and HyparbExecutor.runtime.bin (${#runtime} hex)"
    exit 0
fi

fail=0
if [ "$creation" != "$(cat "$dir/HyparbExecutor.bin")" ]; then
    echo "MISMATCH: creation bytecode differs from contracts/hyparb-executor/HyparbExecutor.bin" >&2
    fail=1
fi
if [ "$runtime" != "$(cat "$dir/HyparbExecutor.runtime.bin")" ]; then
    echo "MISMATCH: runtime bytecode differs from contracts/hyparb-executor/HyparbExecutor.runtime.bin" >&2
    fail=1
fi
if [ "$fail" -eq 0 ]; then
    echo "OK: HyparbExecutor bytecode reproduces from source (solc $ver)"
fi
exit "$fail"
