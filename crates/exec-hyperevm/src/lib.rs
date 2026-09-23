// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # exec-hyperevm (HYPARB H7c) — the HyperEVM write path, TESTNET ONLY
//!
//! Everything between a decided AMM swap and a reconciled receipt:
//!
//! * [`calldata`] — the executor contract's `swap(...)` call (O-H18),
//!   rendered into a stack buffer the transaction BORROWS;
//! * [`gas`] — the G2 bid: a fixed fraction of the expected net edge,
//!   never above the artifact's p99 cap;
//! * [`nonce`] — a fixed multi-wallet nonce table, single-writer, one
//!   transaction in flight per wallet (wallets are the parallelism);
//! * [`rpc`] — the JSON-RPC writers and the fail-closed scanners
//!   (`sendRawTransaction`, receipts, counts, balance, fee history);
//! * [`arm`] — the send arm: sign, render into the request body, post
//!   over one keep-alive HTTPS connection ([`core_net::HttpsPost`]),
//!   and reconcile every receipt against what was signed.
//!
//! ## The interlock (plan §11.4 layer 4, O-H5)
//!
//! [`EVM_ARM_CHAIN_IDS`] is the compiled capability list, mirroring
//! `exec_boot::LIVE_ARM_VENUES`: **998 and nothing else**, held there by
//! a compile-time assertion. [`Network`] has no mainnet variant, so an
//! arm for chain 999 cannot be constructed; [`arm_network`] refuses the
//! id from configuration, and the arm refuses an endpoint whose
//! `eth_chainId` disagrees. Deleting the assertion is the moment the
//! engine becomes able to spend mainnet gas — a phase after H9 says so
//! in writing, or it does not happen. [`check_chains`] is layer 5 plus
//! the O-H12 hybrid: market data and writes on the same chain, or reads
//! on 999 with writes on 998 when (and only when) the hybrid switch is
//! set — never the inverse.
//!
//! ## Doctrine
//!
//! Zero allocation after boot: the request body, the response buffer
//! and the TLS state are allocated when the arm is built; the calldata
//! is a stack array; the signed transaction is rendered as hex straight
//! into the request body (`signer_evm::tx_encode_signed_hex`); every
//! scanner walks the response bytes in place. The arm is BLOCKING (one
//! request/response cycle per call, bounded by
//! [`core_net::https_post::REQ_DEADLINE`]) and therefore lives on its
//! own thread, never the engine loop. Fail-fast: a node that answers a
//! different hash than the one signed, or a receipt from another
//! sender, halts the arm.

#![forbid(unsafe_code)]
#![deny(
    missing_docs,
    unused_imports,
    unused_must_use,
    unreachable_pub,
    clippy::missing_safety_doc
)]

pub mod arm;
pub mod calldata;
pub mod gas;
pub mod nonce;
pub mod rpc;

/// HyperEVM mainnet.
pub const HYPEREVM_MAINNET_CHAIN_ID: u64 = 999;
/// HyperEVM testnet.
pub const HYPEREVM_TESTNET_CHAIN_ID: u64 = 998;

/// **O-H5.** The chain ids the write path may sign for. Testnet only,
/// until a phase after H9 says otherwise in writing.
pub const EVM_ARM_CHAIN_IDS: &[u64] = &[HYPEREVM_TESTNET_CHAIN_ID];
const _: () = assert!(EVM_ARM_CHAIN_IDS.len() == 1 && EVM_ARM_CHAIN_IDS[0] == 998);

/// The networks the write path can name. **Mainnet is deliberately
/// absent** — an arm for chain 999 is not expressible.
#[repr(u64)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Network {
    /// HyperEVM testnet, chain 998.
    Testnet = HYPEREVM_TESTNET_CHAIN_ID,
}

impl Network {
    /// The EIP-155 chain id every transaction is signed for.
    #[inline]
    #[must_use]
    pub const fn chain_id(self) -> u64 {
        self as u64
    }
}

/// Why a chain configuration was refused.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ChainRefusal {
    /// The write chain is not in [`EVM_ARM_CHAIN_IDS`].
    NotArmable {
        /// The refused chain id.
        chain_id: u64,
    },
    /// The market-data and write chains disagree outside the one
    /// hybrid the switch permits (or the switch is set without it).
    Mismatch {
        /// Chain the market data (the pool ingress) reads.
        read: u64,
        /// Chain the arm writes to.
        write: u64,
        /// Whether the O-H12 hybrid switch was set.
        hybrid: bool,
    },
}

impl core::fmt::Display for ChainRefusal {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match *self {
            Self::NotArmable { chain_id } => write!(
                f,
                "evm write path: chain id {chain_id}{} is not in EVM_ARM_CHAIN_IDS [998] — the \
                 write path is TESTNET ONLY (O-H5); a mainnet chain id needs a phase after H9 \
                 that says so in writing",
                if chain_id == HYPEREVM_MAINNET_CHAIN_ID {
                    " (HYPEREVM MAINNET)"
                } else {
                    ""
                }
            ),
            Self::Mismatch {
                read,
                write,
                hybrid,
            } => write!(
                f,
                "evm write path: market data reads chain {read} and the arm writes chain {write} \
                 (hybrid switch {}) — the chains must match, or be exactly reads on 999 with \
                 writes on 998 WITH the hybrid switch (O-H12); never the inverse",
                if hybrid { "SET" } else { "not set" }
            ),
        }
    }
}

impl std::error::Error for ChainRefusal {}

/// Layer 4: the network a chain id may be armed on, or a refusal.
#[inline]
pub fn arm_network(chain_id: u64) -> Result<Network, ChainRefusal> {
    let mut i = 0;
    while i < EVM_ARM_CHAIN_IDS.len() {
        if EVM_ARM_CHAIN_IDS[i] == chain_id {
            return Ok(Network::Testnet);
        }
        i += 1;
    }
    Err(ChainRefusal::NotArmable { chain_id })
}

/// Layer 5 + O-H12: the write chain must be armable, and the read chain
/// must equal it — or, with `hybrid`, be exactly mainnet reads with
/// testnet writes. A hybrid switch set on a same-chain configuration is
/// refused too: a switch that does not describe the configuration is a
/// misconfiguration, not a harmless extra.
pub fn check_chains(
    read_chain: u64,
    write_chain: u64,
    hybrid: bool,
) -> Result<Network, ChainRefusal> {
    let net = arm_network(write_chain)?;
    let same = read_chain == write_chain;
    let the_hybrid =
        read_chain == HYPEREVM_MAINNET_CHAIN_ID && write_chain == HYPEREVM_TESTNET_CHAIN_ID;
    if (same && !hybrid) || (the_hybrid && hybrid) {
        return Ok(net);
    }
    Err(ChainRefusal::Mismatch {
        read: read_chain,
        write: write_chain,
        hybrid,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// DONE(H7): the crate refuses chain id 999.
    #[test]
    fn chain_999_is_refused_and_named_as_mainnet() {
        let e = arm_network(HYPEREVM_MAINNET_CHAIN_ID).unwrap_err();
        assert_eq!(e, ChainRefusal::NotArmable { chain_id: 999 });
        let s = e.to_string();
        assert!(
            s.contains("HYPEREVM MAINNET") && s.contains("TESTNET ONLY"),
            "{s}"
        );
        assert_eq!(
            arm_network(1),
            Err(ChainRefusal::NotArmable { chain_id: 1 })
        );
        assert_eq!(arm_network(998), Ok(Network::Testnet));
        assert_eq!(Network::Testnet.chain_id(), 998);
    }

    #[test]
    fn chains_match_or_are_exactly_the_hybrid() {
        assert_eq!(check_chains(998, 998, false), Ok(Network::Testnet));
        assert_eq!(check_chains(999, 998, true), Ok(Network::Testnet), "O-H12");
        let refused = [
            (999, 998, false), // the hybrid without its switch
            (998, 998, true),  // the switch without the hybrid
            (998, 999, true),  // the inverse: writes on mainnet
            (999, 999, false), // mainnet writes
            (1, 998, true),    // reads on a chain the hybrid does not name
        ];
        let mut i = 0;
        while i < refused.len() {
            let (r, w, h) = refused[i];
            assert!(check_chains(r, w, h).is_err(), "{r}->{w} hybrid={h}");
            i += 1;
        }
        assert_eq!(
            check_chains(998, 999, true),
            Err(ChainRefusal::NotArmable { chain_id: 999 }),
            "the allow-list is checked first"
        );
        let s = check_chains(999, 998, false).unwrap_err().to_string();
        assert!(
            s.contains("never the inverse") && s.contains("not set"),
            "{s}"
        );
    }
}
