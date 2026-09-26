// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Anton (darkcite)

//! # multivenue-engine — main entrypoint
//!
//! Thin dispatch shim. All orchestration logic lives in the `cli`
//! library so that it can be unit-tested without spawning a real
//! binary.
//!
//! Subcommands:
//! * `run --paper` — spawn the ingress threads (Polymarket, Binance,
//!   OKX when `--okx-symbols` is set, Deribit when
//!   `--deribit-symbols` is set, Hyperliquid when `--hl-coins` is
//!   set, Polygon RPC), boot the
//!   real `Engine` with the composed strategy set + paper dispatcher,
//!   drain consumers on the main thread until SIGINT.
//! * `print-config` — load `.env` + env and print the resolved
//!   (non-secret) config.
//! * `exec-smoke` — the E3 TESTNET-ONLY signature gate. Proves this
//!   binary can produce a signature the venue verifies AND that the
//!   venue rejects a corrupted one. It cannot reach mainnet: it reads
//!   its own `HYPERLIQUID_TESTNET_*` variables and refuses any
//!   configuration that is not the testnet host + testnet source.

use std::path::PathBuf;
use std::process::ExitCode;

use std::sync::atomic::AtomicBool;

use clap::Parser;
use cli::{
    boot_info, engine_loop_ev_full, engine_loop_rule_tree_full, engine_loop_set_full,
    install_sigint_handler, join_reverse, spawn_binance, spawn_deribit, spawn_hyperliquid,
    spawn_okx, spawn_polymarket, spawn_rpc, state_writer, Consumers, EngineConfig,
    EngineLoopResult, LatencyDump, LiveDispatcher, Observability, Rings, StrategyPair, WssEndpoint,
    SHUTDOWN,
};
use core_config::{Config, Secrets};
use core_net::TlsTransport;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

/// BIN15 O5 (2026-09-12): the composed-set names this binary will boot.
///
/// This list MUST mirror `strategy_set::mask_for_name` exactly (HYPARB
/// H0 retired the one exemption: `latency-arb` and its standalone arm
/// left with the member, O-H1). It lives here as a named const rather than as match literals so the
/// `strategy_name_pin` tests below can read it: the arm and the mask
/// table DID drift once. The five bin15 names reached `mask_for_name`
/// and the wrapper allow-list but never the match arm, so
/// `--strategy ai+vrp+xsd+bin15` refused the boot at 2026-09-12T20:04:42Z
/// and the whole set — ai-exec, vm, vrp, xsd and bin15 — went dark
/// behind a live capture that looked healthy.
const STRATEGY_SET_NAMES: &[&str] = &[
    "all",
    "ai",
    "ai-exec",
    "vm",
    "icdp",
    "ai+icdp",
    "vrp",
    "ai+vrp",
    "xsd",
    "ai+xsd",
    "ai+vrp+xsd",
    "bin15",
    "ai+bin15",
    "ai+vrp+bin15",
    "ai+xsd+bin15",
    "ai+vrp+xsd+bin15",
    // HYPARB H0: slot 0. Resolves, but refuses the boot as "no
    // requested member is configured" until H5 lands its artifact
    // (lands DARK — O-H8).
    "hyparb",
    "ai+hyparb",
    "ai+vrp+xsd+bin15+hyparb",
];

/// Top-level CLI.
#[derive(Debug, Parser)]
#[command(name = "multivenue-engine", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Debug, Parser)]
// The CLI dispatch enum is parsed once at boot. The size delta
// between `Run` and `PrintConfig` is irrelevant — the value lives
// on the stack of `main` for the whole process. Boxing would
// fight clap's Parser derive for no runtime benefit.
#[allow(clippy::large_enum_variant)]
enum Cmd {
    /// Launch the engine.
    Run(RunArgs),
    /// Print resolved configuration and exit — smoke-tests the `.env` loader.
    PrintConfig(ConfigArgs),
    /// Offline §6.5 capture audit: per-symbol rates, cadence-band
    /// checks, integrity re-derivations and the venue×channel coverage
    /// matrix over one capture run directory.
    AuditReplay(AuditReplayArgs),
    /// Offline 8h backtest harness (docs/phase-8h-design.md §3–§5):
    /// deterministic replay of PMLR capture through the real
    /// strategy-vm against a candidate ruleset. schema-1 JSON on
    /// stdout (the frozen claude-worker contract), human summary on
    /// stderr; exit 0 only when a trustworthy report was printed.
    /// H1 slice: hold-model accounting — the §4 fill model lands in H2.
    Backtest(BacktestArgs),
    /// Offline M3 capture catalog (mvp-plan §4-M3): walks a replay
    /// root (or one `run-<epoch_ns>` dir) and reports per-run wall
    /// spans, per-venue tick coverage, UTC-day continuity (gap map,
    /// gap-free-day streaks), run-dir sizes, the backtest view
    /// (harness §3.1 acceptance + §4.5 day arithmetic) and the
    /// monitor view (§8.3 trailing-window coverage). JSON on stdout,
    /// human summary on stderr; an EMPTY root is a valid zero-run
    /// report (init-if-empty visibility).
    CaptureCatalog(CaptureCatalogArgs),
    /// M4.2 shadow-P&L: replay LOGGED order intents
    /// (engine-orders.pmlr) through the §4 strict-cross fill model —
    /// per-strategy / per-ruleset-hash modeled P&L beside the paper
    /// view. JSON on stdout, human summary on stderr.
    AuditPnl(AuditPnlArgs),
    /// E3 execution gate: sign a probe action against Hyperliquid
    /// TESTNET and assert the venue verifies it — then assert it
    /// REJECTS a deliberately corrupted one.
    ///
    /// Costs nothing and needs no balance, because the probe is a
    /// CANCEL, and a cancel can only ever reduce exposure — never
    /// open a position. What is under test is the signature, not the
    /// order.
    ///
    /// TESTNET ONLY, by construction. There is no flag that points
    /// this at production.
    ExecSmoke(ExecSmokeArgs),
    /// HYPARB H8: the HyperEVM write path's operator verbs, TESTNET
    /// (chain 998) ONLY by construction — `status`, `fund` (derived
    /// wallets from wallet 0), `deploy` (the O-H18 executor), `mint` (a
    /// testnet token's public faucet, to the executor), `battery`
    /// (DONE(H8): a swap lands and reconciles; three wallets without a
    /// nonce collision; an underbid observed losing). Reads the wallet
    /// key from the environment and never opens `.env` —
    /// `scripts/evm-testnet.sh` sources it. Report on stdout.
    EvmTestnet(EvmTestnetArgs),
    /// HYPARB L1 (ruling O-HL1): the HyperEVM MAINNET operator verbs —
    /// `status` (read-only), `deploy` (the H9d executor, bytes checked
    /// on chain), `wrap` (HYPE → WHYPE → the executor), `swap` (one
    /// executor swap on an artifact pool), `sweep` (executor → the
    /// wallet). Every mainnet WRITE needs `--confirm`; `--network
    /// testnet` runs the same verb on chain 998 against `[testnet]`.
    /// Reads keys from the environment and never opens `.env` —
    /// `scripts/evm-live.sh` sources it. Report on stdout.
    EvmLive(EvmLiveArgs),
}

/// `evm-live` verbs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum LiveVerb {
    /// Chain, wallet X, and the executor's code, owner and balances.
    Status,
    /// Deploy the H9d executor from X.
    Deploy,
    /// Wrap `--amount-wei` HYPE into WHYPE and hand it to the executor.
    Wrap,
    /// One executor swap: `--pool`, `--zero-for-one`, `--amount-raw`,
    /// `--min-out-raw`.
    Swap,
    /// The executor returns `--amount-raw` of `--token` to X.
    Sweep,
    /// HYPARB L5: slot 0's live arm end to end — boot, reconcile, a
    /// refused unfundable swap, one AMM swap, one `--coin` hedge round
    /// trip, retirements, the settled equity. `--network testnet` is
    /// the rehearsal; mainnet trades a few dollars and needs `--confirm`.
    ArmSmoke,
}

/// `evm-live --network`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum LiveNetArg {
    /// Chain 999 — real money.
    Mainnet,
    /// Chain 998 — the dry run.
    Testnet,
}

#[derive(Debug, Parser)]
struct EvmLiveArgs {
    /// The verb.
    #[arg(value_enum)]
    verb: LiveVerb,
    /// The chain. MAINNET unless stated.
    #[arg(long, value_enum, default_value = "mainnet")]
    network: LiveNetArg,
    /// The artifact whose `[mainnet]` / `[testnet]` block to use
    /// (default `~/multivenue/hyparb.toml`).
    #[arg(long)]
    hyparb: Option<PathBuf>,
    /// Required by every mainnet write: this send spends real money.
    #[arg(long, default_value_t = false)]
    confirm: bool,
    /// `wrap`: HYPE to wrap and hand to the executor, wei.
    #[arg(long)]
    amount_wei: Option<u128>,
    /// `swap`: exact input; `sweep`: the amount returned. Raw units.
    #[arg(long)]
    amount_raw: Option<u128>,
    /// `swap`: the least output accepted, raw units (> 0).
    #[arg(long)]
    min_out_raw: Option<u128>,
    /// `swap`: the pool (one of the artifact's `[[pool]]`s).
    #[arg(long)]
    pool: Option<String>,
    /// `swap`: token0 in (true) or token1 in (false).
    #[arg(long)]
    zero_for_one: Option<bool>,
    /// `sweep`: the token; `status`: extra executor balances (repeatable).
    #[arg(long)]
    token: Vec<String>,
    /// `arm-smoke`: the hedge coin (one of the artifact's `[[coin]]`s).
    #[arg(long, default_value = "HYPE")]
    coin: String,
    /// `arm-smoke`: the PREFLIGHT — boot, reconcile and the refused
    /// unfundable swap only; nothing is sent to either venue.
    #[arg(long, default_value_t = false)]
    no_trade: bool,
    /// `arm-smoke`: also check this exec.toml arms slot 0 with
    /// `--arm-live` (the engine's own interlock, run without the engine).
    #[arg(long)]
    exec: Option<PathBuf>,
    /// `arm-smoke --exec`: the slots the engine would be armed with.
    #[arg(long)]
    arm_live: Option<String>,
}

/// `evm-testnet` verbs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum EvmVerb {
    /// Chain check; every wallet's address, nonces and balance.
    Status,
    /// Top every derived wallet up to `--amount-wei` from wallet 0.
    Fund,
    /// Deploy the executor from wallet 0 (its owner).
    Deploy,
    /// `--token`'s public `mint(executor, --amount-raw)`.
    Mint,
    /// The DONE(H8) battery.
    Battery,
    /// The engine's shadow (boot, hybrid chain checks, ring, thread) end
    /// to end on synthetic decisions — without stopping the live engine.
    ShadowSmoke,
}

#[derive(Debug, Parser)]
struct EvmTestnetArgs {
    /// The verb.
    #[arg(value_enum)]
    verb: EvmVerb,
    /// The artifact whose `[testnet]` block to use (default
    /// `~/multivenue/hyparb.toml`).
    #[arg(long)]
    hyparb: Option<PathBuf>,
    /// `fund`: each derived wallet's target balance, wei.
    #[arg(long)]
    amount_wei: Option<u128>,
    /// `mint`: the testnet token (0x + 40 hex).
    #[arg(long)]
    token: Option<String>,
    /// `mint`: raw units to mint to the executor.
    #[arg(long)]
    amount_raw: Option<u128>,
    /// `shadow-smoke`: synthetic decisions to shadow.
    #[arg(long, default_value_t = 3)]
    decisions: u64,
    /// `shadow-smoke`: the READ endpoint (the pool ingress's) — default
    /// `https://$HYPEREVM_WS_HOST/`, else the O-H15 archive endpoint.
    #[arg(long)]
    read_url: Option<String>,
}

#[derive(Debug, Parser)]
struct ExecSmokeArgs {
    /// Venue asset id. For the signature probe the cancel is expected
    /// to fail on the ORDER rather than the signature, so the default
    /// is fine; for `--lifecycle` this is the market the order is
    /// placed in and you must state it (LAW E-4: an asset id is bound,
    /// never derived).
    #[arg(long, default_value_t = 0u32)]
    asset: u32,
    /// Run ONLY the offline self-test: reproduce the 25 SDK
    /// known-answer vectors and rebuild one action per type from
    /// inputs, asserting the venue's exact msgpack.
    ///
    /// No network, no credentials, no venue. This is the form CI runs
    /// on every commit touching `exec-hyperliquid` / `signer-eip712`,
    /// because CI has no testnet key and should not have one — and it
    /// is the half that catches LAW E-3 across ALL action types, which
    /// the network probe structurally cannot (it sends one action).
    #[arg(long, default_value_t = false)]
    offline: bool,

    /// Also run PHASE C: place -> modify -> cancel-by-cloid -> verify
    /// the order is really gone (plan §5.1).
    ///
    /// Opt-in and run by a person, never by the restart gate: it costs
    /// balance and creates state on the account. Needs a FUNDED
    /// testnet account with a registered agent wallet.
    ///
    /// The order is post-only, so a price that would cross is REFUSED
    /// by the venue rather than filled. Nothing is derived: you state
    /// the market and both prices, because LAW E-4 forbids deriving an
    /// asset id and a price this code guessed would be the one number
    /// able to turn a test into a trade.
    #[arg(long, default_value_t = false, conflicts_with = "offline")]
    lifecycle: bool,

    /// Phase C resting price, 1e8-scaled (5000000 = 0.05). Put it far
    /// enough from the book that a post-only order rests.
    #[arg(long, requires = "lifecycle")]
    px: Option<i64>,

    /// Phase C price to modify to, 1e8-scaled.
    #[arg(long, requires = "lifecycle")]
    px2: Option<i64>,

    /// Phase C size, 1e8-scaled (1000000000 = 10).
    #[arg(long, requires = "lifecycle")]
    sz: Option<i64>,

    /// Sell instead of buy. For `--lifecycle` this rests on the ask
    /// rather than the bid (the default BID is the side that rests
    /// safely BELOW the market); for `--fill` it crosses the bid.
    #[arg(long, default_value_t = false)]
    sell: bool,

    /// Show exactly what phase C WOULD send, and send nothing.
    ///
    /// Phase C is the only thing here that can create state on a
    /// funded account, and its inputs are four numbers typed on a
    /// command line — a transposed price or a size off by a decimal is
    /// a plausible mistake and an expensive one. This prints the three
    /// actions, with every number rendered the way the VENUE will read
    /// it rather than the way it was typed.
    ///
    /// Needs no key, no network and no account.
    ///
    /// Works for `--fill` too, and matters MORE there: a post-only
    /// order with a mistyped price is refused by the venue, while an
    /// IoC with a mistyped price trades. The fill preview prints the
    /// NOTIONAL, which is the number a misplaced decimal corrupts.
    #[arg(long, default_value_t = false)]
    dry_run: bool,

    /// PHASE D: place IoCs that are meant to TRADE — one by default,
    /// up to `--fill-repeat 32` — and report what the venue ACKED.
    ///
    /// **This spends testnet balance on purpose.** It is the only
    /// thing here that proves our own cloid survives the round trip
    /// and comes back down `userFills` — LAW E-9's attribution rests
    /// on that, and without this probe it rests on source code.
    ///
    /// An IoC either trades or is gone, so unlike `--lifecycle` this
    /// carries no cleanup path — a property of the ORDER TYPE, not one
    /// this code enforces. If the venue rests one anyway, the exit is
    /// nonzero and both ends of the cloid range are printed, because
    /// every cloid is deterministic from `--fill-slot` and
    /// `--fill-cloid`. A batch that STOPS still reports everything it
    /// did: a refusal never hides a fill, or a rest, behind it.
    ///
    /// You state the market, the price and the size: the price must
    /// CROSS the book, and this code does not read the book and will
    /// not guess it. Capped at $100 notional PER ORDER and 32 orders
    /// per invocation — typo limits, not risk limits. Rehearse with
    /// `--dry-run` first; it prints both the per-order and the batch
    /// total.
    ///
    /// Per LAW E-5 the ACK is not the fill. Read `userFills` (or run
    /// `--watch` in another shell) for that.
    #[arg(long, default_value_t = false, conflicts_with_all = ["offline", "lifecycle"])]
    fill: bool,

    /// Phase D crossing price, 1e8-scaled. Must cross the book.
    #[arg(long, requires = "fill")]
    fill_px: Option<i64>,

    /// Phase D size, 1e8-scaled.
    #[arg(long, requires = "fill")]
    fill_sz: Option<i64>,

    /// Phase D slot the cloid names (LAW E-9), so the echo decodes the
    /// way a live fill would.
    #[arg(long, default_value_t = 3, requires = "fill")]
    fill_slot: u8,

    /// Phase D client order id — what `Fill::order_id` must carry when
    /// the fill is booked. With `--fill-repeat` this is the FIRST id;
    /// each order gets the next one.
    #[arg(long, default_value_t = 1, requires = "fill")]
    fill_cloid: u64,

    /// Phase D: place this many IoCs, back to back.
    ///
    /// E4's exit gate wants reconciliation agreeing over >= 20 fills,
    /// and one invocation per fill is twenty chances to mistype a
    /// number. The per-order notional ceiling still applies to EACH
    /// order, so twenty small fills stay twenty small fills — and the
    /// batch has its own bound. Each order carries its own cloid, so
    /// every fill is distinguishable in `userFills`.
    ///
    /// The first refusal ENDS the batch: a venue refusing one order
    /// will likely refuse the rest for the same reason.
    #[arg(long, default_value_t = 1, requires = "fill")]
    fill_repeat: u32,

    /// PHASE G: prove the ROLL SWEEP against the venue (LAW E-8).
    ///
    /// On a roll the arm must take back every order it placed on the
    /// leg that just ended. It asks the VENUE what is resting rather
    /// than trusting a table it keeps, because a local record
    /// disagrees INVISIBLY — which is exactly how a restart or a
    /// missed ACK leaves a quote on a dead instance.
    ///
    /// That path lives in `HlExchange` and E7 gates constructing one,
    /// so this probe drives the same pieces: `frontendOpenOrders`, and
    /// `recon::ours_on_leg` — **the arm's own selection, not a copy**.
    ///
    /// Five requests, post-only: place with our magic cloid, enumerate
    /// (must LIST it, with the cloid, and the selection must PICK it),
    /// cancel by oid as the sweep does, enumerate again (must be GONE).
    /// The leg's name is taken from the venue's own row, never derived
    /// from the asset id.
    #[arg(long, default_value_t = false, conflicts_with_all = ["offline", "lifecycle", "fill", "recon", "requote"])]
    sweep: bool,

    /// PHASE F: prove a requote is a MODIFY that may CHANGE the cloid.
    ///
    /// LAW E-7 makes a live requote a modify rather than a cancel plus
    /// a place — two requests instead of one, at 333 reprices per
    /// instance. The 2026-09-16 ruling added the second half: the
    /// replacement carries a FRESH client id, so every `userFills` row
    /// maps to exactly one quote.
    ///
    /// **Neither half has been measured.** `--lifecycle` targets the
    /// resting order by its VENUE oid and reuses the cloid, so nothing
    /// here has ever asked the venue whether a modify addressed BY
    /// CLOID can also change it. If it cannot, the ruling cannot be
    /// implemented as written.
    ///
    /// Four requests, post-only throughout, and the last two ARE the
    /// assertion: cancelling the old id must be REFUSED and cancelling
    /// the new one must SUCCEED. Neither half proves it alone — a
    /// refusal on the old is equally explained by a modify that created
    /// nothing, and a success on the new by one that left BOTH resting.
    ///
    /// Self-cleaning: the verification is the cleanup. Uses the same
    /// `--px` / `--px2` / `--sz` as `--lifecycle`.
    #[arg(long, default_value_t = false, conflicts_with_all = ["offline", "lifecycle", "fill", "recon"])]
    requote: bool,

    /// PHASE E: run the RECONCILIATION against the live venue.
    ///
    /// §6.2 calls reconciliation "the single most valuable safety net
    /// in the plan", and until this existed it could only run inside a
    /// live `HlExchange` — which E7 gates. So the check meant to catch
    /// a lost fill, a double-counted fill and a wrong asset id had
    /// never once run against a socket.
    ///
    /// **Reads only.** It takes the `userFills` snapshot, rebuilds this
    /// arm's ledger from OUR OWN cloids, asks the venue what it holds,
    /// and compares. No order, no signature, no balance spent — so
    /// unlike every other phase here it needs no A/B probe first.
    ///
    /// Exits nonzero on ANY drift. The worst disagreement is reported
    /// twice: as a contract quantity, and in USD through the settle
    /// ceiling, because a halt threshold is written in money.
    #[arg(long, default_value_t = false, conflicts_with_all = ["offline", "lifecycle", "fill"])]
    recon: bool,

    /// How long to wait for the `userFills` SNAPSHOT, seconds.
    ///
    /// A snapshot that never arrives is an ERROR, never an empty
    /// ledger: an empty ledger would make the comparison pass by
    /// having nothing to compare.
    #[arg(long, default_value_t = 20, requires = "recon")]
    recon_secs: u64,

    /// Subscribe to the USER-EVENT stream (userFills + orderUpdates)
    /// for this many seconds and report what arrives.
    ///
    /// Read-only: it opens a second socket, subscribes for the MASTER
    /// address and prints. It places nothing. Run it alongside
    /// `--lifecycle` or `--fill` in ANOTHER SHELL to watch an order
    /// appear on the venue's own stream — it conflicts with them here,
    /// because `--watch` returns before either runs and a combined
    /// invocation would silently place nothing.
    #[arg(long, conflicts_with_all = ["lifecycle", "fill"])]
    watch: Option<u64>,
}

#[derive(Debug, Parser)]
struct AuditReplayArgs {
    /// Capture run directory (`<MULTIVENUE_LOG_DIR>/run-<ns>`).
    #[arg(long)]
    dir: PathBuf,
}

#[derive(Debug, Parser)]
struct CaptureCatalogArgs {
    /// Replay root (`MULTIVENUE_LOG_DIR`) or one `run-<epoch_ns>`
    /// directory — the same §3.1 resolution as `backtest
    /// --replay-dir`.
    #[arg(long)]
    dir: PathBuf,
    /// Max dark ns a UTC day may carry and still count gap-free
    /// (default 300 s — the daily-restart drain allowance).
    #[arg(long, default_value_t = cli::capture_catalog::DEFAULT_GAP_TOLERANCE_NS)]
    gap_tolerance_ns: u64,
}

#[derive(Debug, Parser)]
struct AuditPnlArgs {
    /// Replay root (`MULTIVENUE_LOG_DIR`) or one `run-<epoch_ns>`
    /// directory — the same §3.1 resolution as `backtest
    /// --replay-dir`.
    #[arg(long)]
    dir: PathBuf,
    /// Repeatable `--fee-bps <venue>[.<class>[.open|.settle]]:<maker>:<taker>`
    /// overrides (same grammar as the backtest arm; XSD-F: a bare venue
    /// sets every class, `<venue>.<class>` one of
    /// spot|perp|dated|option|prediction; BIN15 `.open` = the
    /// charge-once opening pair; E7 `.settle` = the pair charged on a
    /// binary settlement's payout).
    #[arg(long)]
    fee_bps: Vec<String>,
    /// Global activation-Δ override, ns (same as backtest).
    #[arg(long)]
    latency_ns: Option<u64>,
    /// Repeatable `--latency-ns-venue <venue>:<ns>` overrides.
    #[arg(long)]
    latency_ns_venue: Vec<String>,
    /// VT4: repeatable `--stale-after-ms <venue>:<ms>` — the harness
    /// re-judges every v3 tick from its venue stamp against this table
    /// (defaults = the venue table; 0 = never stale).
    #[arg(long)]
    stale_after_ms: Vec<String>,
    /// VRP V2b: repeatable `--opt-fee <venue>:<index_bps>:<prem_bps>`
    /// — the venue's CAPPED option trade fee, `min(index_bps of the
    /// index notional, prem_bps of the premium)`. `<venue>:off` takes
    /// the flat `--fee-bps` path. Absent = Deribit 3:1250, all others
    /// off.
    #[arg(long)]
    opt_fee: Vec<String>,
    /// VRP V3: `--option-spread-frac <ppm>` — the ASSUMED crossed
    /// option spread, parts-per-million of premium (`50000` = 5 %),
    /// charged as half on each side of the D-7 synthetic mark tick.
    /// Absent = 0 = the D-7 floor alone, the optimistic rung. The
    /// option spread is ASSUMED, never measured: the capture carries a
    /// top-of-book quote and a mark, never depth, so there is no size
    /// behind the touch to cross. Max 1000000 (100 %).
    #[arg(long)]
    option_spread_frac: Option<u32>,
    /// RG3: `<path>` = a `regime.toml` artifact to replay the regime
    /// detector from (refusals fatal); `off` = regime-blind; absent =
    /// the default artifact when it exists and resolves on this root.
    #[arg(long)]
    regime: Option<String>,
    /// RG3: `regime-seed.tsv` for the detector's warm-up (default:
    /// the first run directory's own `regime-seed.tsv`, else warm live).
    #[arg(long)]
    regime_seed: Option<PathBuf>,
}

#[derive(Debug, Parser)]
struct BacktestArgs {
    /// Candidate ruleset JSON artifact (8g §4.1 grammar). Required
    /// unless `--member` names a coded member (Tier 3).
    #[arg(long, required_unless_present = "member", conflicts_with = "member")]
    ruleset: Option<PathBuf>,
    /// Tier 3 (statarb doc 08 §6.2): drive a CODED member through the
    /// harness instead of the ruleset VM — `icdp` (with `--icdp <toml>`;
    /// default `~/multivenue/icdp.toml`), `xsd`, `vrp`, `bin15`.
    /// Additive: the frozen worker argv never passes it.
    #[arg(long)]
    member: Option<String>,
    /// `--member icdp`: the parameter artifact (`icdp.toml`).
    #[arg(long, requires = "member")]
    icdp: Option<PathBuf>,
    /// `--member xsd`: the parameter artifact (`xsd.toml`; default
    /// `~/multivenue/xsd.toml`).
    #[arg(long, requires = "member")]
    xsd: Option<PathBuf>,
    /// `--member xsd`: the target / partner / β table (default
    /// `~/multivenue/xsd-table.tsv`); descriptors resolve against the
    /// capture's newest manifest.
    #[arg(long, requires = "member")]
    xsd_table: Option<PathBuf>,
    /// `--member xsd`: hourly closes that warm the z rings (default
    /// `~/multivenue/xsd-seed.tsv`; absent = the member warms from the
    /// replay alone — rows at or after the replay's first hour drop).
    #[arg(long, requires = "member")]
    xsd_seed: Option<PathBuf>,
    /// `--member bin15`: the parameter artifact (`bin15.toml`; default
    /// `~/multivenue/bin15.toml`).
    #[arg(long, requires = "member")]
    bin15: Option<PathBuf>,
    /// `--member bin15`: the directory holding `bin15-seed-<COIN>.tsv`
    /// and `bin15-seed-<COIN>-1d.tsv`. Default = the FIRST run
    /// directory, so a replay is a closed world; pass
    /// `~/multivenue` to fold the live cut in deliberately.
    #[arg(long, requires = "member")]
    bin15_seed_dir: Option<PathBuf>,
    /// `--member hyparb`: the parameter artifact (`hyparb.toml`; default
    /// `~/multivenue/hyparb.toml`).
    #[arg(long, requires = "member")]
    hyparb: Option<PathBuf>,
    /// `--member hyparb`: the `universe.toml` whose `[hyperevm] pools`
    /// names the pools (default `~/multivenue/universe.toml` — the list
    /// is append-only, so the live file names every pool an older
    /// capture carries).
    #[arg(long, requires = "member")]
    hyparb_universe: Option<PathBuf>,
    /// Capture source: a single `run-<epoch_ns>` directory or a log
    /// root (`MULTIVENUE_LOG_DIR`) containing `run-*` children.
    #[arg(long)]
    replay_dir: PathBuf,
    /// IS/OOS split `N/M`: integers, `N + M == 100`, both >= 10 — or
    /// the carved all-OOS monitor form `0/100` (design §3.4). Echoed
    /// verbatim into the schema-1 report.
    #[arg(long)]
    split: String,
    /// §4.3 fee override `<venue>[.<class>[.open|.settle]]:<maker_bps>:<taker_bps>`,
    /// repeatable (venues: pm|bn|okx|deribit|hl|bybit; XSD-F classes:
    /// spot|perp|dated|option|prediction — a bare venue sets all five;
    /// `.open` the charge-once opening pair, `.settle` the pair charged
    /// on a binary settlement's payout). Defaults all 0/0.
    #[arg(long)]
    fee_bps: Vec<String>,
    /// §4.4 global latency-penalty override in ns (default per-venue:
    /// pm 200 ms, bn/okx/deribit 100 ms, hl 600 ms). Parsed but
    /// UNUSED by the H1 hold model (consumed from H2).
    #[arg(long)]
    latency_ns: Option<u64>,
    /// §4.4 per-venue latency override `<venue>:<ns>`, repeatable;
    /// wins over `--latency-ns`. Parsed but UNUSED by the H1 hold
    /// model (consumed from H2).
    #[arg(long)]
    latency_ns_venue: Vec<String>,
    /// VT4: repeatable `--stale-after-ms <venue>:<ms>` — the harness
    /// re-judges every v3 tick from its venue stamp against this table
    /// (defaults = the venue table; 0 = never stale). v2 roots are
    /// stale-blind and say so on stderr.
    #[arg(long)]
    stale_after_ms: Vec<String>,
    /// VRP V2b: repeatable `--opt-fee <venue>:<index_bps>:<prem_bps>`
    /// — the venue's CAPPED option trade fee, `min(index_bps of the
    /// index notional, prem_bps of the premium)`. `<venue>:off` takes
    /// the flat `--fee-bps` path. Absent = Deribit 3:1250, all others
    /// off.
    #[arg(long)]
    opt_fee: Vec<String>,
    /// VRP V3: `--option-spread-frac <ppm>` — the ASSUMED crossed
    /// option spread, parts-per-million of premium (`50000` = 5 %),
    /// charged as half on each side of the D-7 synthetic mark tick.
    /// Absent = 0 = the D-7 floor alone, the optimistic rung. The
    /// option spread is ASSUMED, never measured: the capture carries a
    /// top-of-book quote and a mark, never depth, so there is no size
    /// behind the touch to cross. Max 1000000 (100 %).
    #[arg(long)]
    option_spread_frac: Option<u32>,
    /// §5 rich-detail sidecar path (per-symbol/IS metrics). Declared
    /// now; the sidecar is written starting H2.
    #[arg(long)]
    emit_detail: Option<PathBuf>,
    /// RG3: `<path>` = a `regime.toml` artifact to replay the regime
    /// detector from (refusals fatal); `off` = every row evaluates as
    /// ANY (the on/off delta); absent = the default artifact when it
    /// exists and resolves on this root, else regime-blind (labelled
    /// rows fail closed, stderr says so).
    #[arg(long)]
    regime: Option<String>,
    /// RG3: `regime-seed.tsv` for the detector's warm-up (default:
    /// the first run directory's own `regime-seed.tsv`, else warm live).
    #[arg(long)]
    regime_seed: Option<PathBuf>,
    /// `--member vrp`: the parameter artifact (`vrp.toml`; default
    /// `~/multivenue/vrp.toml`).
    #[arg(long)]
    vrp: Option<PathBuf>,
    /// `--member vrp`: the worker-written boot seed — the settled
    /// `(x, y)` pairs the member's forecast is fitted from. Default =
    /// the FIRST run directory's own `vrp-seed.tsv` when it exists (the
    /// window cut writes one), else a cold boot: LEGAL, and the member
    /// holds until it has 60 pairs. An explicit path that does not
    /// exist, or any file that does not parse exactly, refuses the run.
    #[arg(long)]
    vrp_seed: Option<PathBuf>,
    /// `funding-seed.tsv` replayed through the vm's live `FundingSeed`
    /// path before the first record (default: the first run directory's
    /// own `funding-seed.tsv` when it exists, else none — the funding
    /// features then warm from the window's own prints).
    #[arg(long)]
    funding_seed: Option<PathBuf>,
}

#[derive(Debug, Parser)]
struct RunArgs {
    /// Path to a .env file; defaults to `./.env` via dotenvy.
    #[arg(long)]
    env_file: Option<PathBuf>,
    /// Paper mode — do not actually submit orders. Default ON.
    /// Mutually exclusive with `--live`.
    #[arg(long, default_value_t = true, conflicts_with = "live")]
    paper: bool,
    /// Live mode — sign + POST orders to Polymarket's CLOB. Requires
    /// a valid `.env` with `POLYMARKET_EIP712_KEY`. Default OFF.
    ///
    /// DEPRECATED (E1): this is the legacy PROCESS-WIDE switch and it
    /// can only arm every member or none. Per-strategy routing lives in
    /// `--exec`, which this conflicts with — mixing a global live flag
    /// with a per-slot route table is ambiguous by construction.
    #[arg(long, default_value_t = false, conflicts_with = "exec")]
    live: bool,
    /// E1: per-strategy execution artifact (`exec.toml`; grammar in
    /// `exec.toml.example`). ABSENT = every slot paper = the engine's
    /// behaviour before E1, bit for bit.
    ///
    /// This is one of TWO switches; it arms nothing on its own. See
    /// `--arm-live`.
    #[arg(long)]
    exec: Option<PathBuf>,
    /// E1: the slots this command line agrees to arm, e.g.
    /// `--arm-live 3` or `--arm-live 3,5`.
    ///
    /// Must name EXACTLY the set of slots `--exec`'s artifact marks
    /// live. Artifact says live and this omits the slot -> boot
    /// refusal; this names a slot the artifact calls paper -> boot
    /// refusal. Neither switch alone can arm anything, which is the
    /// point: no single edit reaches real money.
    #[arg(long, requires = "exec")]
    arm_live: Option<String>,
    /// E6: boot with these slots already HALTED, e.g. `--halt-slot 3`
    /// or `--halt-slot 3,5`.
    ///
    /// The operator's kill switch, applied before the first tick: a
    /// halted slot refuses every submit and modify, cancels whatever
    /// the arm is holding, and nothing in the process clears it.
    ///
    /// Unlike `--arm-live` this does NOT require `--exec`, and it is
    /// deliberately not half of a two-switch interlock. Arming needs
    /// two deliberate edits because it reaches real money; halting
    /// needs one, because it is the direction that cannot.
    #[arg(long)]
    halt_slot: Option<String>,
    /// Universe config file (M1; TOML subset — see
    /// `universe.toml.example`). Explicit path must exist. Absent:
    /// `~/multivenue/universe.toml` is used IF present, else the
    /// legacy flag-driven boot. Per-venue flags override the file.
    #[arg(long)]
    universe: Option<PathBuf>,
    /// Binance spot symbol (lowercase, no separator), e.g. `btcusdt`.
    /// Legacy default `btcusdt`; with a universe config this flag
    /// OVERRIDES the config's spot list with the one symbol.
    #[arg(long)]
    binance_symbol: Option<String>,
    /// Internal SymbolId for the Binance spot symbol (legacy anchor 7
    /// when unset). With a universe config, requires
    /// `--binance-symbol`.
    #[arg(long)]
    binance_sym_id: Option<u32>,
    /// Internal SymbolId for the first Polymarket market (legacy
    /// anchor 42 when unset). With a universe config, requires
    /// `--polymarket-asset-id`.
    #[arg(long)]
    polymarket_sym_id: Option<u32>,
    /// Comma-separated OKX instIds (e.g. `BTC-USDT,ETH-USD-SWAP`).
    /// The i-th entry (0-based) is allocated SymbolId
    /// `make_symbol_id(Okx, i+1)` — flag order is id order. Empty /
    /// absent = the OKX ingress thread is not started.
    #[arg(long)]
    okx_symbols: Option<String>,
    /// Also subscribe the OKX 400-level `books` channel per
    /// instrument (capture + integrity only, §4.5). Default OFF —
    /// `bbo-tbt` alone feeds the tick lane.
    #[arg(long, default_value_t = false)]
    okx_depth: bool,
    /// Comma-separated Deribit instruments (e.g.
    /// `BTC-PERPETUAL,ETH-PERPETUAL`). The i-th entry (0-based) is
    /// allocated SymbolId `make_symbol_id(Deribit, i+1)` — flag
    /// order is id order. Empty / absent = the Deribit ingress
    /// thread is not started.
    #[arg(long)]
    deribit_symbols: Option<String>,
    /// Also subscribe the Deribit change_id-chained
    /// `book.{instr}.100ms` channel per instrument (capture +
    /// integrity only, §4.5). Default OFF — `quote` alone feeds
    /// the tick lane.
    #[arg(long, default_value_t = false)]
    deribit_depth: bool,
    /// Comma-separated Hyperliquid coins (e.g. `BTC,ETH,#330`;
    /// HIP-4 `#<enc>` outcome coins and spot `@<idx>` pairs are
    /// ordinary items). The i-th entry (0-based) is allocated
    /// SymbolId `make_symbol_id(Hyperliquid, i+1)` — flag order is
    /// id order. Empty / absent = the Hyperliquid ingress thread is
    /// not started. There is no depth flag: `l2Book` is always
    /// subscribed — it feeds the §6.2 staleness monitor.
    #[arg(long)]
    hl_coins: Option<String>,
    /// VT2: repeatable `--stale-after-ms <venue>:<ms>` — per-venue
    /// tick staleness threshold overriding the measured defaults
    /// (`VenueId::default_stale_after_ms`: pm 1000, bn 1000, okx 400,
    /// deribit 600, hl 700, bybit 500). `0` = measure only, never
    /// flag. Consumed by the venues that stamp venue time (OKX since
    /// VT2; the rest as their extraction lands).
    #[arg(long)]
    stale_after_ms: Vec<String>,
    /// Polymarket CLOB asset id (token id) — the decimal string from
    /// the market's `clobTokenIds`. REQUIRED in legacy mode (no
    /// universe config): without it the PM symbol map is empty and
    /// every Polymarket frame fails lookup (defect D1 — zero PM
    /// ticks); boot refuses to start rather than run venue-blind.
    /// With a universe config, this flag OVERRIDES the config's
    /// market list with the one market.
    #[arg(long)]
    polymarket_asset_id: Option<String>,
    /// Trigger threshold in 1e6 fixed-point units (e.g. `20000` is
    /// $0.02).
    #[arg(long, default_value_t = 20_000i64)]
    threshold_1e6: i64,
    /// Order quantity in 1e6 fixed-point units.
    #[arg(long, default_value_t = 10_000_000i64)]
    qty_1e6: i64,
    /// Cooldown between emits per market, in nanoseconds.
    #[arg(long, default_value_t = 250_000_000u64)]
    cooldown_ns: u64,
    /// Polygon RPC path (e.g. `/v2/<KEY>`). If absent, the RPC ingress
    /// thread is not started.
    #[arg(long)]
    polygon_path: Option<String>,
    /// HYPARB H3b: HyperEVM JSON-RPC WebSocket path on
    /// `HYPEREVM_WS_HOST` (e.g. `/`). Absent ⇒ the pool-event ingress is
    /// not started; present with an empty `[hyperevm] pools` ⇒ warned and
    /// not started.
    #[arg(long)]
    hyperevm_path: Option<String>,
    /// Bind `127.0.0.1:9191` and expose `/metrics` (Prometheus text),
    /// `/healthz` and `/state` (RG6: the 1 s engine snapshot as
    /// JSON — boot identity, regime words, slots, vm rows, recent
    /// orders/fills). Default ON.
    #[arg(long, default_value_t = true)]
    metrics: bool,
    /// Render a live ratatui dashboard over the same 1 s snapshot
    /// `/state` serves. Implies `--metrics`.
    #[arg(long, default_value_t = false)]
    tui: bool,
    /// Strategy selector. `ai` (default since HYPARB H0 retired the
    /// standalone `latency-arb` arm) composes ai-exec + vm through the
    /// set path. `ev` uses Strategy A:
    /// model-vs-market mispricing against claude-worker artifacts.
    /// `ai-exec` (Phase 8f item 8) runs the AI-driven fair-value/
    /// intent strategy alone via the set path (no boot symbol
    /// config — the AI publishes the universe over UDS); paper-only
    /// until 8i. `vm` (Phase 8g) runs the ruleset-VM strategy alone
    /// via the set path (no boot config — it boots inert and trades
    /// only after a ruleset table is staged + committed over UDS,
    /// design §7.3); paper-only until 8i. `all` (Phase 8f) runs the
    /// composed StrategySet: every built member whose config flags
    /// are present (ai-exec and vm need none and are always
    /// included), AI-toggleable at runtime; paper-only until 8i.
    #[arg(long, default_value = "ai")]
    strategy: String,
    /// Path to claude-worker NDJSON tag artifacts. Required when
    /// `--strategy ev`.
    #[arg(long)]
    artifacts_path: Option<PathBuf>,
    /// Path to claude-worker rule JSON. Required for
    /// `--strategy rule-tree`.
    ///
    /// The STANDALONE rule-tree loop only. Slot 3 of the strategy set
    /// is `strategy-bin15` since BIN15 O4b — `strategy-rule-tree` is
    /// unlinked from the set, though the crate still builds and this
    /// bare loop still runs.
    #[arg(long)]
    rules_path: Option<PathBuf>,
    /// BIN15 O4b: the slot-3 parameter artifact
    /// (`~/multivenue/bin15.toml` by default). Read only when the
    /// requested mask carries the bin15 bit (`--strategy bin15` /
    /// `ai+bin15` / … / `all`); an absent or unresolvable artifact
    /// refuses the boot with the bit set — never a silent no-op.
    #[arg(long)]
    bin15: Option<PathBuf>,
    /// BIN15 O4b: directory holding `bin15-seed-<COIN>.tsv`
    /// (`~/multivenue/` by default). An ABSENT seed is legal — a cold
    /// boot must be — and the member holds until its HAR window warms.
    #[arg(long)]
    bin15_seed_dir: Option<PathBuf>,
    /// HYPARB H5: the slot-0 parameter artifact
    /// (`~/multivenue/hyparb.toml` by default). Read only when the
    /// requested mask carries slot 0 (`--strategy hyparb` / `ai+hyparb`
    /// / … / `all`); absent or unresolvable with the bit set REFUSES the
    /// boot — never a silent no-op. The member also needs its pools:
    /// `--hyperevm-path` and a non-empty `[hyperevm] pools`.
    #[arg(long)]
    hyparb: Option<PathBuf>,
    /// HYPARB O-H5: the second switch of the EVM write path, TESTNET
    /// ONLY (chain 998; the crate refuses 999 regardless). Must agree
    /// with the artifact's `mode = "testnet"` — both or neither.
    #[arg(long, default_value_t = false)]
    evm_testnet: bool,
    /// HYPARB O-H12: the third switch — the pool ingress may read chain
    /// 999 (mainnet signal) while the write path writes chain 998. Only
    /// with `--evm-testnet`; never the inverse. Shouted in the ARMED tell.
    #[arg(long, default_value_t = false, requires = "evm_testnet")]
    evm_hybrid: bool,
    /// ICDP I5: the slot-6 parameter artifact (`~/multivenue/icdp.toml`
    /// by default). Read only when the requested mask carries the icdp
    /// bit (`--strategy icdp` / `ai+icdp` / `all`); an absent or
    /// unresolvable artifact refuses the boot with the bit set — never
    /// a silent no-op.
    #[arg(long)]
    icdp: Option<PathBuf>,
    /// RG2: the regime detector's parameter artifact
    /// (`~/multivenue/regime.toml` by default; `docs/regime-and-dashboard-plan.md`
    /// §4.6). Set boots only. An ABSENT default file boots the detector
    /// UNCONFIGURED (every word UNKNOWN, unconstrained members open —
    /// today's behaviour, boot tell `regime: no artifact`); an explicit
    /// `--regime <path>` or a present-but-invalid default refuses the
    /// boot — never a silent no-op.
    #[arg(long)]
    regime: Option<PathBuf>,
    /// RG2: the worker-written minute-close seed (`~/multivenue/regime-seed.tsv`
    /// by default, plan §4.3) — read only when the detector configured;
    /// absent = warm live (boot tell `regime: seed absent`).
    #[arg(long)]
    regime_seed: Option<PathBuf>,
    /// VRP V5: the worker-written boot seed
    /// (`~/multivenue/vrp-seed.tsv` by default) — the settled `(x, y)`
    /// pairs the VRP member's forecast is fitted from. Absent is LEGAL
    /// (cold boot; the member holds until it has 60 pairs); an explicit
    /// path that does not exist, or any file that does not parse
    /// exactly, refuses the boot.
    #[arg(long)]
    vrp_seed: Option<PathBuf>,
    /// VRP V7: `vrp.toml` — the VRP member's parameter artifact
    /// (`~/multivenue/vrp.toml` by default). ABSENT at the default
    /// location = the member is not configured and its enable bit is
    /// never set (the `icdp.toml` law); an explicit path that does not
    /// exist, or any file that does not parse, refuses the boot.
    #[arg(long)]
    vrp: Option<PathBuf>,
    /// F22: `vrp-state.tsv` — the VRP member's OWN persisted state (the
    /// pairs it formed, the QLIKE window, an open campaign). Default:
    /// beside the `--vrp` artifact when that was explicit, else
    /// `~/multivenue/vrp-state.tsv`. The path used to be hard-wired, so
    /// any `--vrp <other.toml>` smoke boot read AND REWROTE the standing
    /// engine's state.
    #[arg(long)]
    vrp_state: Option<PathBuf>,
    /// XSD-3: `xsd.toml` — the cross-sectional member's parameter
    /// artifact (`~/multivenue/xsd.toml` by default). ABSENT at the
    /// default location = the member is not configured and its enable
    /// bit is never set (the `icdp.toml` law); an explicit path that
    /// does not exist, or a file that does not parse, refuses the boot.
    #[arg(long)]
    xsd: Option<PathBuf>,
    /// XSD-3: `xsd-table.tsv` — the worker's monthly target / partner /
    /// β table (`~/multivenue/xsd-table.tsv` by default). Absent at the
    /// default location = not configured (same as no `xsd.toml`); rows
    /// whose descriptors are not in the boot universe are dropped and
    /// counted; the file's sha256 is the table identity the state file
    /// must match.
    #[arg(long)]
    xsd_table: Option<PathBuf>,
    /// XSD-3: `xsd-seed.tsv` — hourly closes that warm the z rings
    /// (`~/multivenue/xsd-seed.tsv` by default). Absent is LEGAL (the
    /// member warms live); rows at or after the boot hour are dropped.
    #[arg(long)]
    xsd_seed: Option<PathBuf>,
    /// XSD-3: `xsd-state.tsv` — the engine's own persisted positions
    /// (`~/multivenue/xsd-state.tsv` by default), restored under the
    /// same table hash, flattened under a changed one; a row the boot
    /// universe cannot name refuses the boot.
    #[arg(long)]
    xsd_state: Option<PathBuf>,
    /// Cadence in seconds for periodic HdrHistogram dumps. `0`
    /// disables dumping (default). When >0, the engine writes the
    /// three latency histograms (ingest→strategy, strategy→submit,
    /// submit→ack) into `--latency-dump-dir` every N seconds.
    #[arg(long, default_value_t = 0u64)]
    latency_dump_secs: u64,
    /// Destination directory for HdrHistogram dumps. Defaults to
    /// `<MULTIVENUE_LOG_DIR>/latency`. Only consulted when
    /// `--latency-dump-secs` is non-zero.
    #[arg(long)]
    latency_dump_dir: Option<PathBuf>,
    /// Comma-separated venue labels (`pm,bn,okx,rpc,deribit,hl`) or
    /// the literal `all` — enables the §6.5 bounded raw-payload tap
    /// for those ingress threads. Default: none (tap off everywhere).
    #[arg(long)]
    raw_tap: Option<String>,
    /// Raw-tap recording mode: `rejects` (only parser-rejected
    /// payloads) or `all` (every inbound payload, rejects included).
    /// Only meaningful when `--raw-tap` names at least one venue.
    #[arg(long, default_value = "rejects")]
    raw_tap_mode: String,
    /// Raw-tap file budget per venue, in MiB. Once exhausted, further
    /// tap records are dropped and counted
    /// (`PmlrCapture::tap_dropped`) rather than growing the file.
    #[arg(long, default_value_t = 64u64)]
    raw_tap_budget_mb: u64,
}

#[derive(Debug, Parser)]
struct ConfigArgs {
    /// Path to a .env file; defaults to `./.env` via dotenvy.
    #[arg(long)]
    env_file: Option<PathBuf>,
}

fn main() -> ExitCode {
    // Parse BEFORE installing tracing: the backtest arm must route
    // every log line to stderr — its stdout is the schema-1 JSON the
    // worker `json.loads`es, and one stray fmt-layer line (default
    // writer: stdout) would corrupt the frozen contract. The other
    // arms keep their historical stdout logging unchanged.
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Run(args) => {
            init_tracing();
            run(args)
        }
        Cmd::PrintConfig(args) => {
            init_tracing();
            print_config(args)
        }
        Cmd::AuditReplay(args) => {
            init_tracing();
            audit_replay(args)
        }
        Cmd::Backtest(args) => {
            init_tracing_stderr();
            backtest(args)
        }
        Cmd::CaptureCatalog(args) => {
            // stderr tracing for the same reason as the backtest arm:
            // stdout carries the catalog JSON and nothing else.
            init_tracing_stderr();
            capture_catalog(args)
        }
        Cmd::AuditPnl(args) => {
            // Same stdout-purity law: JSON only on stdout.
            init_tracing_stderr();
            audit_pnl(args)
        }
        Cmd::ExecSmoke(args) => {
            // Same law again: one line of JSON on stdout for CI, the
            // human report on stderr.
            init_tracing_stderr();
            exec_smoke(args)
        }
        Cmd::EvmTestnet(args) => {
            init_tracing_stderr();
            evm_testnet(args)
        }
        Cmd::EvmLive(args) => {
            init_tracing_stderr();
            evm_live(args)
        }
    }
}

/// HYPARB L1: the `evm-live` verbs. Exit 0 only when the verb did what
/// it says (`status`: every check held; `swap`: mined without a revert).
fn evm_live(args: EvmLiveArgs) -> ExitCode {
    use cli::evm_live as el;
    let net = match args.network {
        LiveNetArg::Mainnet => el::LiveNet::Mainnet,
        LiveNetArg::Testnet => el::LiveNet::Testnet,
    };
    let write = args.verb != LiveVerb::Status;
    // O-HL1: a mainnet write's authority is the operator's --confirm.
    let auth = if write && net == el::LiveNet::Mainnet {
        match exec_hyperevm::MainnetAuthority::operator_verb(args.confirm) {
            Ok(a) => Some(a),
            Err(e) => {
                eprintln!("evm-live: {e}");
                return ExitCode::from(2);
            }
        }
    } else {
        None
    };
    let path = match args.hyparb.clone() {
        Some(p) => p,
        None => match core_config::hyparb::default_hyparb_path() {
            Ok(p) => PathBuf::from(p),
            Err(e) => {
                eprintln!("evm-live: {e}");
                return ExitCode::from(2);
            }
        },
    };
    let (file, target) = match core_config::hyparb::load(&path)
        .map_err(|e| e.to_string())
        .and_then(|(f, _)| el::Target::from_file(&f, net).map(|t| (f, t)))
    {
        Ok(ft) => ft,
        Err(e) => {
            eprintln!("evm-live: {}: {e}", path.display());
            return ExitCode::from(2);
        }
    };
    let mut tokens = Vec::with_capacity(args.token.len());
    let mut i = 0usize;
    while i < args.token.len() {
        match cli::evm_testnet::parse_addr(&args.token[i]) {
            Ok(a) => tokens.push(a),
            Err(e) => {
                eprintln!("evm-live: --token {e}");
                return ExitCode::from(2);
            }
        }
        i += 1;
    }
    let tls = TlsTransport::default_client_config();
    let a = auth.as_ref();
    let out = match args.verb {
        LiveVerb::Status => el::verb_status(&target, &tokens, tls),
        LiveVerb::Deploy => el::verb_deploy(&target, a, tls).map(|r| (r, true)),
        LiveVerb::Wrap => match args.amount_wei {
            Some(w) => el::verb_wrap(&target, a, w, tls).map(|r| (r, true)),
            None => Err("wrap needs --amount-wei".to_owned()),
        },
        LiveVerb::Swap => match (
            args.pool.as_deref().map(cli::evm_testnet::parse_addr),
            args.zero_for_one,
            args.amount_raw,
            args.min_out_raw,
        ) {
            (Some(Ok(pool)), Some(z), Some(amt), Some(min)) => {
                el::verb_swap(&target, a, pool, z, amt, min, tls)
            }
            (Some(Err(e)), ..) => Err(format!("--pool {e}")),
            _ => Err(
                "swap needs --pool, --zero-for-one, --amount-raw and --min-out-raw".to_owned(),
            ),
        },
        LiveVerb::Sweep => match (tokens.as_slice(), args.amount_raw) {
            ([tok], Some(amt)) => el::verb_sweep(&target, a, *tok, amt, tls).map(|r| (r, true)),
            _ => Err("sweep needs exactly one --token and --amount-raw".to_owned()),
        },
        LiveVerb::ArmSmoke => {
            let interlock = match args.exec.as_deref() {
                None => Ok(String::new()),
                Some(x) => match cli::exec_boot::resolve(Some(x), args.arm_live.as_deref()) {
                    Ok(Some(eb)) if eb.hyparb_live() => Ok(format!(
                        "interlock: {} + --arm-live {} arms slot 0 — OK\n",
                        x.display(),
                        args.arm_live.as_deref().unwrap_or("")
                    )),
                    Ok(_) => Err(format!(
                        "{} with --arm-live {:?} does not arm slot 0",
                        x.display(),
                        args.arm_live
                    )),
                    Err(e) => Err(format!("{}: {e}", x.display())),
                },
            };
            let pool = args.pool.as_deref().map(cli::evm_testnet::parse_addr).transpose();
            match (interlock, pool) {
                (Err(e), _) => Err(e),
                (_, Err(e)) => Err(format!("--pool {e}")),
                (Ok(head), Ok(p)) => cli::hyparb_rehearsal::verb_arm_smoke(
                    &target,
                    &file,
                    a,
                    &args.coin,
                    p,
                    args.no_trade,
                    tls,
                )
                .map(|(r, ok)| (head + &r, ok)),
            }
        }
    };
    match out {
        Ok((report, ok)) => {
            print!("{report}");
            if ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Err(e) => {
            eprintln!("evm-live: {e}");
            ExitCode::from(1)
        }
    }
}

/// HYPARB H8: the `evm-testnet` verbs. Exit 0 only when the verb did
/// what it says (the battery: only when DONE(H8) held).
fn evm_testnet(args: EvmTestnetArgs) -> ExitCode {
    use cli::evm_testnet as et;
    let path = match args.hyparb.clone() {
        Some(p) => p,
        None => match core_config::hyparb::default_hyparb_path() {
            Ok(p) => PathBuf::from(p),
            Err(e) => {
                eprintln!("evm-testnet: {e}");
                return ExitCode::from(2);
            }
        },
    };
    let file = match core_config::hyparb::load(&path) {
        Ok((f, _)) => f,
        Err(e) => {
            eprintln!("evm-testnet: {e}");
            return ExitCode::from(2);
        }
    };
    let Some(t) = file.testnet.as_ref() else {
        eprintln!("evm-testnet: {} has no [testnet] block", path.display());
        return ExitCode::from(2);
    };
    let tls = TlsTransport::default_client_config();
    let out = match args.verb {
        EvmVerb::Status => et::verb_status(t, tls).map(|r| (r, true)),
        EvmVerb::Fund => match args.amount_wei {
            Some(a) => et::verb_fund(t, a, tls).map(|r| (r, true)),
            None => Err("fund needs --amount-wei".to_owned()),
        },
        EvmVerb::Deploy => et::verb_deploy(t, tls).map(|r| (r, true)),
        EvmVerb::Mint => match (args.token.as_deref(), args.amount_raw) {
            (Some(tok), Some(a)) => et::verb_mint(t, tok, a, tls).map(|r| (r, true)),
            _ => Err("mint needs --token and --amount-raw".to_owned()),
        },
        EvmVerb::Battery => et::verb_battery(t, tls),
        EvmVerb::ShadowSmoke => {
            let read_url = args.read_url.clone().unwrap_or_else(|| {
                let host = std::env::var("HYPEREVM_WS_HOST")
                    .unwrap_or_else(|_| "rpc.purroofgroup.com".to_owned());
                format!("https://{host}/")
            });
            et::verb_shadow_smoke(t, file.gas_p99_usd_1e6, &read_url, args.decisions, tls)
        }
    };
    match out {
        Ok((report, ok)) => {
            print!("{report}");
            if ok {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            }
        }
        Err(e) => {
            eprintln!("evm-testnet: {e}");
            ExitCode::from(1)
        }
    }
}

/// E3 arm: the TESTNET-ONLY signature gate.
///
/// Exit codes are `exec_hyperliquid::EXIT_*` and `scripts/exec-smoke.sh`
/// branches on them. Any non-zero code refuses an armed restart — a
/// relinked binary that cannot sign a testnet order must never be
/// allowed to sign a mainnet one.
///
/// It reads the process environment and never opens `.env` itself:
/// the wrapper sources it. Code that opened the operator's secrets
/// file would be one refactor away from logging it.
fn exec_smoke(args: ExecSmokeArgs) -> ExitCode {
    use exec_hyperliquid::{HlConfig, Scope};

    // The offline half needs nothing: not a key, not a host, not a
    // socket. Do it before anything can fail for a reason that has
    // nothing to do with this binary's signing.
    if args.offline {
        return match exec_hyperliquid::smoke::self_test_only() {
            Ok(r) => {
                println!(
                    "{{\"selftest_rows\":{},\"selftest_encoders\":{},\"passed\":true}}",
                    r.rows, r.encoders
                );
                info!(
                    rows = r.rows,
                    encoders = r.encoders,
                    "exec-smoke: OFFLINE SELF-TEST PASSED — this binary reproduces the venue \
                     SDK's bytes for every action type"
                );
                ExitCode::SUCCESS
            }
            Err(e) => {
                error!("{e}");
                ExitCode::from(e.code() as u8)
            }
        };
    }

    // Validate phase C's arguments BEFORE any network work. An
    // operator who forgot --px should learn that now, not after two
    // venue round-trips have already run.
    if args.lifecycle && (args.px.is_none() || args.px2.is_none() || args.sz.is_none()) {
        error!("exec-smoke: --lifecycle needs --px, --px2 and --sz (all 1e8-scaled)");
        eprintln!(
            "exec-smoke: e.g. --lifecycle --asset 100032530 --px 1000000 --px2 2000000 --sz 1000000000\n\
             exec-smoke: (0.01 -> 0.02, size 10). Post-only, so a price that would cross is \
             refused by the venue rather than filled — but choose one that rests."
        );
        return ExitCode::from(exec_hyperliquid::EXIT_FAILED as u8);
    }

    if args.fill && (args.fill_px.is_none() || args.fill_sz.is_none()) {
        error!("exec-smoke: --fill needs --fill-px and --fill-sz (both 1e8-scaled)");
        eprintln!(
            "exec-smoke: e.g. --fill --asset 100102180 --fill-px 99000000 --fill-sz 100000000\n\
             exec-smoke: (cross at 0.99, size 1). This TRADES — the price must cross the book."
        );
        return ExitCode::from(exec_hyperliquid::EXIT_FAILED as u8);
    }

    // A dry run reaches no venue and needs no credentials, so it
    // happens before anything that could fail for an unrelated reason.
    if args.dry_run && args.fill {
        let spec = exec_hyperliquid::lifecycle::FillSpec {
            asset: args.asset,
            px_1e8: args.fill_px.unwrap_or(0),
            sz_1e8: args.fill_sz.unwrap_or(0),
            is_buy: !args.sell,
            strategy_id: args.fill_slot,
            client_oid: args.fill_cloid,
            repeat: args.fill_repeat,
        };
        return match exec_hyperliquid::lifecycle::preview_fill(spec) {
            Ok(p) => {
                eprintln!(
                    "exec-smoke DRY RUN (phase D) — nothing was sent.\n\
                     \x20 asset {asset}\n\
                     \x20 {side} {sz} @ {px}  =  {notional} USDC per order\n\
                     \x20 x{repeat} orders  =  {batch} USDC total\n\
                     \x20 IoC: this is MEANT TO TRADE, and an IoC is not meant to rest \
                     — so there is nothing to cancel afterwards. Check the totals above \
                     before running it for real\n\
                     \x20 cloids {cloid} .. {last} — look for these in userFills",
                    asset = args.asset,
                    side = if args.sell { "SELL" } else { "BUY" },
                    sz = p.sz,
                    px = p.px,
                    notional = p.notional,
                    repeat = p.repeat,
                    batch = p.batch_notional,
                    cloid = p.cloid,
                    last = p.last_cloid,
                );
                println!("{}", p.place);
                ExitCode::SUCCESS
            }
            Err(e) => {
                error!("{e}");
                ExitCode::from(exec_hyperliquid::EXIT_FAILED as u8)
            }
        };
    }
    if args.dry_run {
        let spec = exec_hyperliquid::LifecycleSpec {
            asset: args.asset,
            px_1e8: args.px.unwrap_or(0),
            px2_1e8: args.px2.unwrap_or(0),
            sz_1e8: args.sz.unwrap_or(0),
            is_buy: !args.sell,
        };
        return match exec_hyperliquid::lifecycle::preview(spec) {
            Ok(p) => {
                eprintln!(
                    "exec-smoke DRY RUN — nothing was sent.\n\
                     \x20 asset {asset}\n\
                     \x20 {side} {sz} @ {px}, then modified to {px2}\n\
                     \x20 post-only (ALO): a price that would cross is REFUSED by the venue, \
                     not filled\n\
                     \x20 the cancel goes by CLOID, so a modify issuing a new oid cannot strand it",
                    asset = args.asset,
                    side = if args.sell { "SELL" } else { "BUY" },
                    sz = p.sz,
                    px = p.px,
                    px2 = p.px2,
                );
                println!("{}", p.place);
                println!("{}", p.modify);
                println!("{}", p.cancel);
                ExitCode::SUCCESS
            }
            Err(e) => {
                error!("{e}");
                ExitCode::from(exec_hyperliquid::EXIT_FAILED as u8)
            }
        };
    }

    let scope = Scope::Testnet;
    if let Some(secs) = args.watch {
        return exec_watch(scope, secs);
    }
    // Beside `--watch`, and before the A/B probe on purpose: this
    // phase signs nothing, so requiring a signature probe to run it
    // would be ceremony rather than a guard.
    if args.recon {
        return exec_recon(scope, args.recon_secs);
    }
    let cfg = match HlConfig::from_env(scope) {
        Ok(c) => c,
        Err(e) => {
            error!("exec-smoke: {e}");
            eprintln!(
                "exec-smoke: this gate reads its OWN variables, disjoint from the live arm's:\n\
                 \x20 {key}   (required) the TESTNET agent/API wallet private key\n\
                 \x20 {addr}  (required) the TESTNET master account address\n\
                 \x20 {host}  (optional, defaults to the testnet host)\n\
                 \x20 {src}   (optional, defaults to \"b\")",
                key = scope.agent_key_var(),
                addr = scope.master_addr_var(),
                host = scope.host_var(),
                src = scope.source_var(),
            );
            return ExitCode::from(exec_hyperliquid::EXIT_FAILED as u8);
        }
    };
    // Debug redacts the key and shows the derived agent address —
    // which is the thing to check against the venue's API page.
    info!(?cfg, "exec-smoke: testnet configuration");

    let tls = TlsTransport::default_client_config();
    match exec_hyperliquid::smoke::run(&cfg, tls, args.asset) {
        Ok(report) => {
            println!("{}", report.to_json());
            if !report.passed() {
                error!("exec-smoke: report did not pass; refusing");
                return ExitCode::from(exec_hyperliquid::EXIT_FAILED as u8);
            }
            info!(
                host = %report.host,
                agent = %report.agent,
                selftest_rows = report.selftest_rows,
                rejected_with = %report.corruption_message,
                "exec-smoke: PASSED — the venue verified this binary's signature and rejected a \
                 corrupted one"
            );
            if args.lifecycle {
                return exec_lifecycle(&cfg, &args);
            }
            if args.fill {
                return exec_fill(&cfg, &args);
            }
            if args.requote {
                return exec_requote(&cfg, &args);
            }
            if args.sweep {
                return exec_sweep(&cfg, &args);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            error!("{e}");
            ExitCode::from(e.code() as u8)
        }
    }
}

/// Watch the user-event stream. Read-only; places nothing.
fn exec_recon(scope: exec_hyperliquid::Scope, secs: u64) -> ExitCode {
    let cfg = match exec_hyperliquid::HlConfig::from_env(scope) {
        Ok(c) => c,
        Err(e) => {
            error!("exec-smoke: {e}");
            return ExitCode::from(exec_hyperliquid::EXIT_FAILED as u8);
        }
    };
    info!(
        host = %cfg.host,
        secs,
        "exec-recon: PHASE E — reading userFills + spotClearinghouseState. Places nothing."
    );
    let tls = TlsTransport::default_client_config();
    let wait = std::time::Duration::from_secs(secs);
    match exec_hyperliquid::lifecycle::run_recon(&cfg, tls, wait) {
        Ok(r) => {
            let l = r.ledger;
            println!(
                "{{\"rows\":{},\"ours\":{},\"foreign\":{},\"settlements\":{},\
                 \"settlements_unowned\":{},\"legs\":{},\"refused\":{},\"balances\":{},\
                 \"drift_legs\":{},\"legs_nonzero\":{},\"venue_legs_unreconciled\":{},\
                 \"worst_qty_1e6\":{},\"worst_usd_1e6\":{},\
                 \"agreed\":{}}}",
                l.rows,
                l.ours,
                l.foreign,
                l.settlements,
                l.settlements_unowned,
                l.legs,
                l.refused,
                r.balances,
                r.drift_legs,
                r.legs_nonzero,
                r.venue_legs_unreconciled,
                r.worst_qty_1e6,
                r.worst_usd_1e6,
                r.agreed(),
            );
            if l.refused > 0 {
                error!(
                    refused = l.refused,
                    "exec-recon: rows the ledger could not place. A reconciliation missing \
                     rows agrees by having less to disagree with — this is NOT a pass."
                );
                return ExitCode::from(exec_hyperliquid::EXIT_LIFECYCLE as u8);
            }
            // Distinguishable from a real disagreement on purpose. Both
            // exit nonzero, but "the ledger and the venue differ" and
            // "there was no ledger" need different things done about
            // them, and folding them into one message is how the second
            // gets read as the first.
            if l.ours == 0 {
                error!(
                    rows = l.rows,
                    foreign = l.foreign,
                    settlements = l.settlements,
                    "exec-recon: the snapshot carried NO fills of ours. Nothing was compared, \
                     so this is not a pass — check HYPERLIQUID_TESTNET_MASTER_ADDR, or that \
                     this account has traded at all."
                );
                return ExitCode::from(exec_hyperliquid::EXIT_LIFECYCLE as u8);
            }
            // A run that agreed on every leg it LOOKED AT, while the
            // venue holds a leg it never looked at, has not reconciled
            // the account. Its own message, because "the ledger and
            // the venue differ" and "this run did not cover everything
            // the account holds" need different things done about them.
            if r.venue_legs_unreconciled > 0 {
                error!(
                    venue_legs_unreconciled = r.venue_legs_unreconciled,
                    legs = l.legs,
                    balances = r.balances,
                    "exec-recon: the venue holds outcome legs this run NEVER COMPARED. Phase E \
                     reaches only the intersection of the userFills snapshot window and our own \
                     fills, so a position whose trades aged out of that window reads as \
                     agreement by absence. This is not a pass."
                );
                return ExitCode::from(exec_hyperliquid::EXIT_LIFECYCLE as u8);
            }
            if !r.agreed() {
                error!(
                    legs = l.legs,
                    drift_legs = r.drift_legs,
                    worst_qty_1e6 = r.worst_qty_1e6,
                    worst_usd_1e6 = r.worst_usd_1e6,
                    "exec-recon: the venue and this arm's ledger DISAGREE. That is a lost \
                     fill, a double-counted fill, a wrong asset id or a stale view — the four \
                     things this check exists to find."
                );
                return ExitCode::from(exec_hyperliquid::EXIT_LIFECYCLE as u8);
            }
            info!(
                legs = l.legs,
                legs_nonzero = r.legs_nonzero,
                ours = l.ours,
                settlements = l.settlements,
                foreign = l.foreign,
                balances = r.balances,
                "exec-recon: AGREED on every leg, and the venue holds no outcome leg this run \
                 did not compare."
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            error!("{e}");
            ExitCode::from(e.code() as u8)
        }
    }
}

fn exec_watch(scope: exec_hyperliquid::Scope, secs: u64) -> ExitCode {
    use exec_hyperliquid::userws::{scan_user_fills, UserFill};
    use exec_hyperliquid::{HlConfig, UserWs};

    let cfg = match HlConfig::from_env(scope) {
        Ok(c) => c,
        Err(e) => {
            error!("exec-smoke: {e}");
            return ExitCode::from(exec_hyperliquid::EXIT_FAILED as u8);
        }
    };
    let tls = TlsTransport::default_client_config();
    let mut ws = match UserWs::new(&cfg.host, 443, tls, &cfg.master_addr) {
        Ok(w) => w,
        Err(e) => {
            error!("{e}");
            return ExitCode::from(exec_hyperliquid::EXIT_UNREACHABLE as u8);
        }
    };
    info!(
        host = %cfg.host,
        master = %ws.master_hex(),
        "exec-watch: subscribing userFills + orderUpdates (the MASTER address, not the agent)"
    );
    if let Err(e) = ws.connect() {
        error!("{e}");
        return ExitCode::from(exec_hyperliquid::EXIT_UNREACHABLE as u8);
    }
    info!("exec-watch: connected and subscribed");

    let end = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    let mut msgs = 0usize;
    let mut fills = [UserFill::default(); exec_hyperliquid::recon::MAX_SPOT_BALANCES];
    while std::time::Instant::now() < end {
        let r = ws.pump(std::time::Duration::from_millis(500), |payload| {
            msgs += 1;
            let ch = channel_of(payload).unwrap_or("?");
            match ch {
                "userFills" => match scan_user_fills(payload, &mut fills) {
                    Ok((n, snap)) => {
                        info!(count = n, snapshot = snap, "exec-watch: userFills");
                        for f in &fills[..n] {
                            info!(
                                tid = f.tid,
                                oid = f.oid,
                                coin = %String::from_utf8_lossy(f.coin.of(payload)),
                                px_1e8 = f.px_1e8,
                                sz_1e8 = f.sz_1e8,
                                ours = ?exec_hyperliquid::userws::owner_of(f),
                                "  fill"
                            );
                        }
                    }
                    Err(e) => error!(?e, "exec-watch: userFills did not scan"),
                },
                other => info!(
                    channel = other,
                    bytes = payload.len(),
                    body = %String::from_utf8_lossy(&payload[..payload.len().min(220)]),
                    "exec-watch: message"
                ),
            }
        });
        if let Err(e) = r {
            error!("{e}");
            return ExitCode::from(exec_hyperliquid::EXIT_UNREACHABLE as u8);
        }
    }
    info!(messages = msgs, "exec-watch: done");
    ExitCode::SUCCESS
}

/// `"channel":"<name>"` out of a venue frame, for the log line.
fn channel_of(p: &[u8]) -> Option<&str> {
    let k = b"\"channel\"";
    let i = (0..p.len().saturating_sub(k.len())).find(|&i| &p[i..i + k.len()] == k)?;
    let rest = &p[i + k.len()..];
    let c = rest.iter().position(|&b| b == b':')?;
    let q1 = rest[c..].iter().position(|&b| b == b'"')? + c + 1;
    let q2 = rest[q1..].iter().position(|&b| b == b'"')? + q1;
    core::str::from_utf8(&rest[q1..q2]).ok()
}

/// Phase D: place ONE IoC meant to TRADE, and report the ACK.
///
/// Runs only after phases A and B, for the same reason phase C does: a
/// trade placed through a signature the venue cannot verify proves
/// nothing about the signature and costs balance to learn it.
fn exec_fill(cfg: &exec_hyperliquid::HlConfig, args: &ExecSmokeArgs) -> ExitCode {
    let (Some(px), Some(sz)) = (args.fill_px, args.fill_sz) else {
        error!("exec-smoke: --fill needs --fill-px and --fill-sz (both 1e8-scaled)");
        return ExitCode::from(exec_hyperliquid::EXIT_FAILED as u8);
    };
    let spec = exec_hyperliquid::lifecycle::FillSpec {
        asset: args.asset,
        px_1e8: px,
        sz_1e8: sz,
        is_buy: !args.sell,
        strategy_id: args.fill_slot,
        client_oid: args.fill_cloid,
        repeat: args.fill_repeat,
    };
    info!(?spec, "exec-smoke: phase D — placing IoCs that are MEANT TO TRADE on testnet");
    let tls = TlsTransport::default_client_config();
    match exec_hyperliquid::lifecycle::run_fill(cfg, tls, spec) {
        Ok(run) => {
            let r = run.report;
            let mut hex = [0u8; 34];
            let n = exec_hyperliquid::cloid::to_hex(&r.cloid, &mut hex);
            let cloid = String::from_utf8_lossy(&hex[..n]).to_string();
            // UNCONDITIONAL, and before every verdict below. A batch
            // that stopped has state on the venue; a caller parsing
            // stdout must see it whatever the exit code turns out to
            // be, and the shape this replaced printed nothing at all
            // on exactly that path.
            println!(
                "{{\"cloid\":\"{cloid}\",\"oid\":{},\"attempted\":{},\"sent\":{},\"filled\":{},\
                 \"any_resting\":{},\"in_doubt\":{},\"stopped\":{}}}",
                r.oid,
                r.attempted,
                r.sent,
                r.filled,
                r.any_resting,
                r.in_doubt,
                run.stopped.is_some()
            );
            // Something MAY be on the book: the venue rested one, or a
            // request went out whose answer we never read. NOT a venue
            // refusal — that IS an answer, and it says nothing was
            // placed. A non-crossing price is the most common outcome
            // of this command, and an alarm that fires on it is an
            // alarm nobody reads by the twentieth run.
            if r.any_resting || r.in_doubt {
                error!(
                    first = %fill_cloid_hex(args.fill_slot, args.fill_cloid),
                    last = %fill_cloid_hex(
                        args.fill_slot,
                        args.fill_cloid
                            .saturating_add(u64::from(r.attempted.saturating_sub(1))),
                    ),
                    "exec-smoke: an order may be ON THE BOOK. Cancel by cloid across the range \
                     above before doing anything else."
                );
            }
            // A DIFFERENT fact with a different trigger: the venue has
            // seen these ids, whether or not it placed anything, and it
            // refuses a duplicate cloid. True of the benign refusal
            // too, which is why it is not folded into the alarm above.
            if r.attempted > 0 && run.stopped.is_some() {
                warn!(
                    "exec-smoke: the venue has already seen client ids {}..={}. Re-run with \
                     --fill-cloid {} or higher, or it will refuse them as duplicates.",
                    args.fill_cloid,
                    args.fill_cloid
                        .saturating_add(u64::from(r.attempted.saturating_sub(1))),
                    args.fill_cloid.saturating_add(u64::from(r.attempted)),
                );
            }
            if r.any_resting {
                error!(?r, "exec-smoke: an IoC RESTED — the venue did what the order type forbids");
                return ExitCode::from(exec_hyperliquid::EXIT_LIFECYCLE as u8);
            }
            if let Some(e) = run.stopped.as_ref() {
                error!(
                    ?r,
                    "exec-smoke: the batch STOPPED at order {} of {} — {e}. The most common \
                     cause is the price not crossing, which the venue refuses outright and \
                     which costs nothing but a retry. What printed above is what actually \
                     happened; this run is NOT >= N fills and must not be counted as one.",
                    r.attempted,
                    args.fill_repeat,
                );
                return ExitCode::from(e.code() as u8);
            }
            if r.filled == 0 {
                // Reached only when the venue ACKED every order with no
                // error, yet none filled and none rested — a status the
                // scanner recognises as neither. The ordinary
                // did-not-cross case never gets here: the venue REFUSES
                // it, so it arrives as `stopped` above, carrying the
                // venue's own words. Fail-closed catch-all.
                error!(
                    ?r,
                    "exec-smoke: every order was ACKED with no error, yet nothing filled and \
                     nothing rested. The venue returned a status this binary does not \
                     recognise — do not count this run, and read the raw answer."
                );
                return ExitCode::from(exec_hyperliquid::EXIT_LIFECYCLE as u8);
            }
            if r.filled != r.sent {
                error!(
                    ?r,
                    "exec-smoke: only some of the batch traded. Nothing was left resting, but \
                     the run is NOT >= N fills and must not be counted as one."
                );
                return ExitCode::from(exec_hyperliquid::EXIT_LIFECYCLE as u8);
            }
            // The next free id, on the SUCCESS path too. The E4 gate
            // wants >= 20 fills, so the natural workflow is run twenty,
            // check reconciliation, run twenty more — and a clean run
            // consumes its ids just as surely as a stopped one. Not an
            // alarm, so it rides with the ACK rather than as a warning.
            info!(
                oid = r.oid,
                filled = r.filled,
                cloid = %cloid,
                next_fill_cloid = args.fill_cloid.saturating_add(u64::from(r.attempted)),
                "exec-smoke: PHASE D ACK — the venue says filled. Per LAW E-5 that is the ACK, \
                 NOT the fill: look for these cloids in userFills. A re-run must pass \
                 --fill-cloid next_fill_cloid or the venue refuses the ids as duplicates."
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            // `run_fill` reserves `Err` for the cases where NOTHING was
            // sent, so there is nothing on the venue to report here.
            error!("{e}");
            ExitCode::from(e.code() as u8)
        }
    }
}

/// One cloid, rendered the way the venue prints it.
///
/// A batch's recovery range is two of these. An operator handed only
/// its head cannot sweep it, which is why both ends are printed.
/// The same, for a cloid this code did not build from a slot and an
/// id — the phase F probe's own, which are a marker plus a timestamp.
fn fill_cloid_hex_raw(c: &[u8; 16]) -> String {
    let mut hex = [0u8; 34];
    let n = exec_hyperliquid::cloid::to_hex(c, &mut hex);
    String::from_utf8_lossy(&hex[..n]).to_string()
}

fn fill_cloid_hex(slot: u8, client_oid: u64) -> String {
    let c = exec_hyperliquid::cloid::encode(slot, client_oid);
    let mut hex = [0u8; 34];
    let n = exec_hyperliquid::cloid::to_hex(&c, &mut hex);
    String::from_utf8_lossy(&hex[..n]).to_string()
}

/// Phase C: the order lifecycle round trip. Runs only AFTER phases A
/// and B, because a lifecycle measured through a signature the venue
/// cannot verify measures nothing.
fn exec_sweep(cfg: &exec_hyperliquid::HlConfig, args: &ExecSmokeArgs) -> ExitCode {
    let (Some(px), Some(sz)) = (args.px, args.sz) else {
        error!("exec-smoke: --sweep needs --px and --sz (both 1e8-scaled)");
        return ExitCode::from(exec_hyperliquid::EXIT_FAILED as u8);
    };
    let spec = exec_hyperliquid::lifecycle::SweepSpec {
        asset: args.asset,
        px_1e8: px,
        sz_1e8: sz,
        is_buy: !args.sell,
        strategy_id: args.fill_slot,
        client_oid: args.fill_cloid,
    };
    info!(
        ?spec,
        "exec-smoke: phase G — proving the ROLL SWEEP against the venue. POST-ONLY."
    );
    let tls = TlsTransport::default_client_config();
    match exec_hyperliquid::lifecycle::run_sweep(cfg, tls, spec) {
        Ok(r) => {
            let cloid = fill_cloid_hex_raw(&r.cloid);
            let coin = String::from_utf8_lossy(&r.coin[..r.coin_len as usize]).to_string();
            // UNCONDITIONAL, before every verdict: a post-only order may
            // be on the book and this line is how it is found.
            println!(
                "{{\"cloid\":\"{cloid}\",\"placed_oid\":{},\"coin\":\"{coin}\",\
                 \"listed\":{},\"selected\":{},\"cancelled\":{},\"gone_after\":{},\
                 \"unswept\":{},\"stopped\":{},\"passed\":{}}}",
                r.placed_oid,
                r.listed,
                r.selected,
                r.cancelled,
                r.gone_after,
                r.unswept,
                r.stopped.is_some(),
                r.passed()
            );
            if r.unswept {
                error!(
                    cloid = %cloid,
                    "exec-smoke: PHASE G — a POST-ONLY order may still be resting under the \
                     cloid above. Cancel it by cloid before running this again."
                );
                return ExitCode::from(exec_hyperliquid::EXIT_LIFECYCLE as u8);
            }
            if let Some(e) = r.stopped.as_ref() {
                error!(cloid = %cloid, "exec-smoke: PHASE G stopped — {e}");
                return ExitCode::from(e.code() as u8);
            }
            if !r.listed {
                error!(
                    cloid = %cloid,
                    "exec-smoke: PHASE G — the venue did NOT list our order with its cloid. A \
                     sweep cannot tell our orders from a stranger's without it, which is the \
                     whole reason it asks for frontendOpenOrders rather than openOrders."
                );
                return ExitCode::from(exec_hyperliquid::EXIT_LIFECYCLE as u8);
            }
            if !r.selected {
                error!(
                    cloid = %cloid,
                    coin = %coin,
                    "exec-smoke: PHASE G — the venue listed it but recon::ours_on_leg did NOT \
                     select it. The arm would walk past this order on every roll."
                );
                return ExitCode::from(exec_hyperliquid::EXIT_LIFECYCLE as u8);
            }
            if !r.gone_after {
                error!(
                    cloid = %cloid,
                    "exec-smoke: PHASE G — the cancel was ACKED and the order is STILL LISTED. \
                     Acked and gone are different claims, which is why this probe asks twice."
                );
                return ExitCode::from(exec_hyperliquid::EXIT_LIFECYCLE as u8);
            }
            if !r.passed() {
                error!(cloid = %cloid, "exec-smoke: PHASE G did not pass");
                return ExitCode::from(exec_hyperliquid::EXIT_LIFECYCLE as u8);
            }
            info!(
                placed_oid = r.placed_oid,
                coin = %coin,
                cloid = %cloid,
                "exec-smoke: PHASE G PASSED — the venue lists our order WITH its cloid, the \
                 arm's own selection picks it out of the whole account, the cancel takes it \
                 off, and a second enumerate confirms it is gone. LAW E-8's sweep is measured."
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            error!("{e}");
            ExitCode::from(e.code() as u8)
        }
    }
}

fn exec_requote(cfg: &exec_hyperliquid::HlConfig, args: &ExecSmokeArgs) -> ExitCode {
    let (Some(px), Some(px2), Some(sz)) = (args.px, args.px2, args.sz) else {
        error!("exec-smoke: --requote needs --px, --px2 and --sz (all 1e8-scaled)");
        return ExitCode::from(exec_hyperliquid::EXIT_FAILED as u8);
    };
    let spec = exec_hyperliquid::LifecycleSpec {
        asset: args.asset,
        px_1e8: px,
        px2_1e8: px2,
        sz_1e8: sz,
        is_buy: !args.sell,
    };
    info!(
        ?spec,
        "exec-smoke: phase F — can a modify BY cloid also CHANGE the cloid? POST-ONLY throughout."
    );
    let tls = TlsTransport::default_client_config();
    match exec_hyperliquid::lifecycle::run_requote(cfg, tls, spec) {
        Ok(r) => {
            let old = fill_cloid_hex_raw(&r.old_cloid);
            let new = fill_cloid_hex_raw(&r.new_cloid);
            // UNCONDITIONAL, and before every verdict. Both ids are
            // derived from a millisecond nobody typed, so a run that
            // printed neither left any stranded order unrecoverable by
            // hand.
            println!(
                "{{\"old_cloid\":\"{old}\",\"new_cloid\":\"{new}\",\"placed_oid\":{},\
                 \"modified_oid\":{},\"old_cancel_refused\":{},\"new_cancel_succeeded\":{},\
                 \"unswept_old\":{},\"unswept_new\":{},\"new_resting\":{},\"old_resting\":{},\
                 \"readback_failed\":{},\"stopped\":{},\"passed\":{}}}",
                r.placed_oid,
                r.modified_oid,
                r.old_cancel_refused,
                r.new_cancel_succeeded,
                r.unswept_old,
                r.unswept_new,
                r.new_resting,
                r.old_resting,
                r.readback_failed,
                r.stopped.is_some(),
                r.passed() && r.confirmed_by_venue()
            );
            if r.has_unswept() {
                error!(
                    old = %old,
                    new = %new,
                    unswept_old = r.unswept_old,
                    unswept_new = r.unswept_new,
                    "exec-smoke: PHASE F — a cancel was never answered. A POST-ONLY order may \
                     be on the book under the id(s) above. Cancel it by cloid before running \
                     this again."
                );
                return ExitCode::from(exec_hyperliquid::EXIT_LIFECYCLE as u8);
            }
            if let Some(e) = r.stopped.as_ref() {
                error!(old = %old, new = %new, "exec-smoke: PHASE F stopped — {e}");
                return ExitCode::from(e.code() as u8);
            }
            if !r.old_cancel_refused && r.new_cancel_succeeded {
                error!(
                    old = %old,
                    "exec-smoke: PHASE F — the modify left BOTH orders resting. That is a LEAK \
                     at the VENUE, not a residue here (both were cancelled): two quotes on the \
                     book under one intent. LAW E-7 cannot be implemented this way."
                );
                return ExitCode::from(exec_hyperliquid::EXIT_LIFECYCLE as u8);
            }
            if r.old_cancel_refused && !r.new_cancel_succeeded {
                error!(
                    new = %new,
                    "exec-smoke: PHASE F — the modify consumed the old order and left NOTHING \
                     under the new id. A requote that can lose the quote is worse than a cancel \
                     plus a place."
                );
                return ExitCode::from(exec_hyperliquid::EXIT_LIFECYCLE as u8);
            }
            // §7.1's own evidence, ahead of the catch-all: the two
            // cancel outcomes above are INFERENCES about what was
            // resting, and this is what the venue actually said.
            if r.readback_failed {
                error!(
                    old = %old,
                    new = %new,
                    "exec-smoke: PHASE F — the orders were swept, but `frontendOpenOrders` \
                     could not be read, so nothing observed the resting state directly. \
                     Fail-closed: an unreadable answer is not a pass."
                );
                return ExitCode::from(exec_hyperliquid::EXIT_LIFECYCLE as u8);
            }
            if !r.new_resting || r.old_resting {
                error!(
                    old = %old,
                    new = %new,
                    new_resting = r.new_resting,
                    old_resting = r.old_resting,
                    "exec-smoke: PHASE F — the venue's own book disagrees with the cancels. \
                     Exactly one order must rest, under the NEW id."
                );
                return ExitCode::from(exec_hyperliquid::EXIT_LIFECYCLE as u8);
            }
            if !r.confirmed_by_venue() {
                error!(
                    old = %old,
                    new = %new,
                    new_resting = r.new_resting,
                    old_resting = r.old_resting,
                    "exec-smoke: PHASE F — the cancels agree but the venue's own book does \
                     not confirm it"
                );
                return ExitCode::from(exec_hyperliquid::EXIT_LIFECYCLE as u8);
            }
            if !r.passed() {
                error!(old = %old, new = %new, "exec-smoke: PHASE F did not pass");
                return ExitCode::from(exec_hyperliquid::EXIT_LIFECYCLE as u8);
            }
            info!(
                placed_oid = r.placed_oid,
                modified_oid = r.modified_oid,
                old = %old,
                new = %new,
                "exec-smoke: PHASE F PASSED — the venue accepts a modify addressed BY cloid that \
                 gives the replacement a DIFFERENT one. The order MOVED: the old id was refused \
                 and the new id was really resting. LAW E-7 with a fresh cloid per requote is \
                 implementable."
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            error!("{e}");
            ExitCode::from(e.code() as u8)
        }
    }
}

fn exec_lifecycle(cfg: &exec_hyperliquid::HlConfig, args: &ExecSmokeArgs) -> ExitCode {
    // Checked at entry, before any network work; this is the
    // unwrap-free restatement of that.
    let (Some(px), Some(px2), Some(sz)) = (args.px, args.px2, args.sz) else {
        error!("exec-smoke: --lifecycle needs --px, --px2 and --sz (all 1e8-scaled)");
        return ExitCode::from(exec_hyperliquid::EXIT_FAILED as u8);
    };
    let spec = exec_hyperliquid::LifecycleSpec {
        asset: args.asset,
        px_1e8: px,
        px2_1e8: px2,
        sz_1e8: sz,
        is_buy: !args.sell,
    };
    info!(?spec, "exec-smoke: phase C — placing a POST-ONLY order on testnet");
    let tls = TlsTransport::default_client_config();
    match exec_hyperliquid::lifecycle::run(cfg, tls, spec) {
        Ok(r) => {
            println!(
                "{{\"placed_oid\":{},\"modified_oid\":{},\"cancelled\":{},\"verified_gone\":{},\"passed\":{}}}",
                r.placed_oid, r.modified_oid, r.cancelled, r.verified_gone, r.passed()
            );
            if !r.passed() {
                error!(?r, "exec-smoke: PHASE C did not pass");
                return ExitCode::from(exec_hyperliquid::EXIT_LIFECYCLE as u8);
            }
            info!(
                placed_oid = r.placed_oid,
                modified_oid = r.modified_oid,
                "exec-smoke: PHASE C PASSED — placed, modified, cancelled by cloid, and confirmed \
                 gone"
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            error!("{e}");
            ExitCode::from(e.code() as u8)
        }
    }
}

/// M4.2 arm: JSON on stdout + human summary on stderr, exit 0 iff a
/// report was produced; any failure prints its reason to stderr only
/// and exits nonzero.
fn audit_pnl(args: AuditPnlArgs) -> ExitCode {
    let cfg = cli::audit_pnl::AuditPnlConfig {
        replay_dir: args.dir,
        fee_bps: args.fee_bps,
        latency_ns: args.latency_ns,
        latency_ns_venue: args.latency_ns_venue,
        stale_after_ms: args.stale_after_ms,
        opt_fee: args.opt_fee,
        option_spread_frac_1e6: args.option_spread_frac,
        regime: cli::backtest::regime::RegimeMode::parse(args.regime.as_deref()),
        regime_seed: args.regime_seed,
    };
    let mut report = |line: &str| eprintln!("{line}");
    match cli::audit_pnl::run(&cfg, &mut report) {
        Ok(json) => {
            println!("{json}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("audit-pnl: {e}");
            ExitCode::from(1)
        }
    }
}

/// M3 catalog arm: JSON on stdout + summary on stderr, exit 0 iff a
/// report was produced (an empty root IS a report); any failure
/// prints its reason to stderr only and exits nonzero.
fn capture_catalog(args: CaptureCatalogArgs) -> ExitCode {
    let cfg = cli::capture_catalog::CatalogConfig {
        dir: args.dir,
        gap_tolerance_ns: args.gap_tolerance_ns,
    };
    match cli::capture_catalog::run_catalog(&cfg) {
        Ok(out) => {
            eprint!("{}", out.summary);
            println!("{}", out.json);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("capture-catalog: {e}");
            ExitCode::from(1)
        }
    }
}

/// The §5 exit-code contract: schema-1 on stdout + summary on stderr
/// and exit 0 iff the report is trustworthy; ANY failure prints its
/// reason to stderr only and exits nonzero (the worker maps every
/// nonzero to `BacktestError` — "harness output untrusted").
fn backtest(args: BacktestArgs) -> ExitCode {
    // Tier 3: `--member <kind>` selects the coded-member arm; the
    // parameter file is the member's own boot artifact.
    let member = match args.member.as_deref() {
        None => None,
        Some(name) => {
            let Some(kind) = cli::backtest::member::MemberKind::parse(name) else {
                eprintln!(
                    "backtest: unknown --member {name:?} (known: icdp, xsd, vrp, bin15, hyparb)"
                );
                return ExitCode::from(1);
            };
            let params = match kind {
                cli::backtest::member::MemberKind::Icdp => match args.icdp.clone() {
                    Some(p) => p,
                    None => match core_config::icdp::default_icdp_path() {
                        Ok(p) => PathBuf::from(p),
                        Err(e) => {
                            eprintln!("backtest: --member icdp needs --icdp <toml>: {e}");
                            return ExitCode::from(1);
                        }
                    },
                },
                cli::backtest::member::MemberKind::Xsd => match args.xsd.clone() {
                    Some(p) => p,
                    None => match core_config::xsd::default_xsd_path() {
                        Ok(p) => PathBuf::from(p),
                        Err(e) => {
                            eprintln!("backtest: --member xsd needs --xsd <toml>: {e}");
                            return ExitCode::from(1);
                        }
                    },
                },
                cli::backtest::member::MemberKind::Vrp => match args.vrp.clone() {
                    Some(p) => p,
                    None => match core_config::vrp::default_vrp_path() {
                        Ok(p) => PathBuf::from(p),
                        Err(e) => {
                            eprintln!("backtest: --member vrp needs --vrp <toml>: {e}");
                            return ExitCode::from(1);
                        }
                    },
                },
                cli::backtest::member::MemberKind::Bin15 => match args.bin15.clone() {
                    Some(p) => p,
                    None => match core_config::bin15::default_bin15_path() {
                        Ok(p) => PathBuf::from(p),
                        Err(e) => {
                            eprintln!("backtest: --member bin15 needs --bin15 <toml>: {e}");
                            return ExitCode::from(1);
                        }
                    },
                },
                cli::backtest::member::MemberKind::Hyparb => match args.hyparb.clone() {
                    Some(p) => p,
                    None => match core_config::hyparb::default_hyparb_path() {
                        Ok(p) => PathBuf::from(p),
                        Err(e) => {
                            eprintln!("backtest: --member hyparb needs --hyparb <toml>: {e}");
                            return ExitCode::from(1);
                        }
                    },
                },
            };
            Some(cli::backtest::member::MemberSpec {
                kind,
                params,
                table: args.xsd_table.clone(),
                seed: args.xsd_seed.clone(),
                vrp_seed: args.vrp_seed.clone(),
                bin15_seed_dir: args.bin15_seed_dir.clone(),
                hyparb_universe: args.hyparb_universe.clone(),
            })
        }
    };
    let cfg = cli::backtest::BacktestConfig {
        ruleset: args.ruleset.unwrap_or_default(),
        replay_dir: args.replay_dir,
        split: args.split,
        fee_bps: args.fee_bps,
        latency_ns: args.latency_ns,
        latency_ns_venue: args.latency_ns_venue,
        stale_after_ms: args.stale_after_ms,
        opt_fee: args.opt_fee,
        option_spread_frac_1e6: args.option_spread_frac,
        emit_detail: args.emit_detail,
        regime: cli::backtest::regime::RegimeMode::parse(args.regime.as_deref()),
        regime_seed: args.regime_seed,
        funding_seed: args.funding_seed,
        member,
    };
    let result = match cfg.member.as_ref() {
        Some(spec) => cli::backtest::member::run_member(&cfg, spec),
        None => cli::backtest::run(&cfg),
    };
    match result {
        Ok(out) => {
            eprint!("{}", out.summary);
            println!("{}", out.schema1);
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("backtest: {e}");
            ExitCode::from(1)
        }
    }
}

fn audit_replay(args: AuditReplayArgs) -> ExitCode {
    match cli::audit_replay::run_audit(&args.dir) {
        Ok(report) => {
            println!("{report}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            error!(error = ?e, dir = %args.dir.display(), "audit-replay failed");
            ExitCode::from(1)
        }
    }
}

fn init_tracing() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}

/// Backtest-arm tracing: identical filter, writer pinned to stderr so
/// stdout carries schema-1 bytes and nothing else.
fn init_tracing_stderr() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_writer(std::io::stderr)
        .try_init();
}

fn print_config(args: ConfigArgs) -> ExitCode {
    let cfg = match Config::load(args.env_file.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            error!(error = ?e, "config load failed");
            return ExitCode::from(1);
        }
    };
    info!(?cfg, "resolved non-secret config");
    match Secrets::load() {
        Ok(_) => info!("secrets loaded (redacted)"),
        Err(e) => info!(error=?e, "secrets not present — that's fine for PrintConfig"),
    }
    ExitCode::SUCCESS
}

/// Boot a [`LiveDispatcher`] from the loaded config + secrets.
/// Returns a static error message on any boot-time failure so the
/// caller can surface a clean `EngineLoopResult::Failed`.
/// **E7 — the operator's Hyperliquid arm**, for every live slot on
/// Hyperliquid except slot 0 (which hedges from its own account). Fed by
/// fill lane 3; configured from the `HYPERLIQUID_*` variables.
fn boot_operator_hl_arm(
    eb: &cli::exec_boot::ExecBoot,
    md_ws_host: &str,
    f3p: core_ring::Producer<core_types::Fill, { engine::FILL_RING_SIZE }>,
) -> Result<exec_hyperliquid::HlExchange<{ engine::FILL_RING_SIZE }>, String> {
    let hl_cfg = exec_hyperliquid::HlConfig::from_env(exec_hyperliquid::Scope::Live).map_err(|e| {
        format!("slot(s) LIVE on hyperliquid but the arm cannot be configured: {e}")
    })?;
    // **LAW E-4's precondition.** The asset ids the arm binds come from
    // the MARKET-DATA ingress's roll events; the orders go to the
    // EXCHANGE host. A testnet id is a mainnet stranger's market and vice
    // versa, so both hosts must be on the same network or the boot
    // refuses.
    let md_testnet = md_ws_host.contains("testnet");
    let ex_testnet = hl_cfg.network == exec_hyperliquid::Network::Testnet;
    if md_testnet != ex_testnet {
        return Err(format!(
            "HYPERLIQUID_WS_HOST ({md_ws_host}) and HYPERLIQUID_EXCHANGE_HOST ({}) are on \
             different networks — the roll events that bind asset ids would name another \
             network's markets (LAW E-4). Point both at testnet or both at mainnet.",
            hl_cfg.host
        ));
    }
    // The tightest floor across the live HL slots this arm trades: the
    // budget is a property of the ADDRESS (slot 0's is its own).
    let floor = eb
        .slots
        .iter()
        .enumerate()
        .filter(|(i, s)| *i != cli::exec_boot::HYPEREVM_SLOT && s.is_live())
        .map(|(_, s)| s.request_budget_floor)
        .filter(|f| *f > 0)
        .min()
        .and_then(|f| u64::try_from(f).ok())
        .unwrap_or(0);
    let budget_path = eb
        .path
        .parent()
        .map(std::path::Path::to_path_buf)
        .unwrap_or_default()
        .join(exec_hyperliquid::budget::DEFAULT_STATE_PATH);
    let mut arm = exec_hyperliquid::HlExchange::new(
        &hl_cfg,
        TlsTransport::default_client_config(),
        f3p,
        budget_path.clone(),
        floor,
    )
    .map_err(|e| e.to_string())?;
    // E7-F1: the address budget is the venue's number, read once here;
    // the cold assumption halted the first mainnet boot before its first
    // order (2026-09-19).
    arm.seed_budget_from_venue();
    // S7-L1 (gap E): the request-weight top-up is a property of the
    // ADDRESS, like the floor — the most conservative of the live slots'
    // numbers (slot 0's address is its own). Each slot's own day ceiling
    // holds at least its own weight (the parser refuses less), so the two
    // minima never arm a top-up that cannot fire.
    let topup_weight = eb
        .slots
        .iter()
        .enumerate()
        .filter(|(i, s)| {
            *i != cli::exec_boot::HYPEREVM_SLOT && s.is_live() && s.request_topup_weight > 0
        })
        .map(|(_, s)| s.request_topup_weight)
        .min()
        .and_then(|w| u64::try_from(w).ok())
        .unwrap_or(0);
    let topup_day_max = eb
        .slots
        .iter()
        .enumerate()
        .filter(|(i, s)| {
            *i != cli::exec_boot::HYPEREVM_SLOT && s.is_live() && s.request_topup_day_max > 0
        })
        .map(|(_, s)| s.request_topup_day_max)
        .min()
        .and_then(|w| u64::try_from(w).ok())
        .unwrap_or(0);
    arm.set_topup(topup_weight, topup_day_max);
    // S7-L1 (gaps A, C): this arm's seeding waits for the venue's day
    // spend and for a read of the account that finds nothing of ours
    // resting.
    arm.enable_restart_safety();
    // S7-L1 (gap C): this process starts every member flat, so every
    // order of ours still resting at the venue is a previous process's
    // orphan — a crash, a `kill -9`, a drain that ran out of time. Taken
    // off before anything trades.
    let (boot_swept, boot_sweep_left) = arm.cancel_ours_everywhere();
    if boot_sweep_left != 0 {
        warn!(
            left = boot_sweep_left,
            "exec: the boot sweep could not confirm every resting order of ours cancelled \
             (u32::MAX = the venue's open orders could not be read) — while any cancellable \
             one may rest, the live slots stay unseeded and each reconciliation retries the \
             sweep"
        );
    }
    info!(
        host = %hl_cfg.host,
        network = if ex_testnet { "testnet" } else { "MAINNET" },
        agent = %exec_hyperliquid::config::hex20(&hl_cfg.agent_addr),
        master = %exec_hyperliquid::config::hex20(&hl_cfg.master_addr),
        budget_floor = floor,
        budget_source = ?arm.budget_source(),
        budget_remaining = arm.budget_remaining(),
        budget_state = %budget_path.display(),
        topup_weight,
        topup_day_max,
        boot_swept,
        boot_sweep_left,
        // E7 session bound: 0 = not anchored yet — the first
        // reconciliation sets it (equity at cost, S7-L1) and writes the
        // file; a restart restores it.
        pnl_anchor_usd_1e6 = arm.pnl_anchor_usd_1e6(),
        pnl_anchor_state = %arm.pnl_state_path().display(),
        "exec: hyperliquid arm ARMED"
    );
    Ok(arm)
}

/// **HYPARB L5 — slot 0's live arm** from the booted artifact, the
/// exec interlock and the universe. The engine's `MainnetAuthority` door
/// opens only on all three switches.
fn boot_hyparb_live_arm(
    eb: &cli::exec_boot::ExecBoot,
    hb: Option<&cli::hyparb_boot::HyparbBoot>,
    universe: &core_config::universe::AllocatedUniverse,
) -> Result<cli::hyparb_live::LiveBoot<{ engine::FILL_RING_SIZE }>, String> {
    let hb = hb.ok_or("slot 0 is armed live but no hyparb artifact is booted")?;
    let authority = exec_hyperevm::MainnetAuthority::armed_engine(
        hb.mode == core_config::hyparb::HyparbMode::Live,
        eb.hyparb_live(),
    )
    .map_err(|e| e.to_string())?;
    let m = hb
        .mainnet
        .as_ref()
        .ok_or("hyparb.toml has no [mainnet] block")?;
    let executor = cli::evm_testnet::parse_addr(
        m.executor
            .as_deref()
            .ok_or("hyparb.toml [mainnet] `executor` is not set")?,
    )?;
    let (pools, coins) =
        cli::hyparb_live::pools_and_coins(hb, &universe.hyperevm, &universe.hyperevm_pools)?;
    let floor = eb
        .slots
        .get(cli::exec_boot::HYPEREVM_SLOT)
        .and_then(|s| u64::try_from(s.request_budget_floor).ok())
        .unwrap_or(0);
    let spec = cli::hyparb_live::LiveSpec {
        net: cli::evm_live::LiveNet::Mainnet,
        authority: Some(&authority),
        evm_endpoint: &m.endpoint,
        executor,
        pools,
        coins,
        gas_coin: hb.params.gas_coin,
        state_dir: cli::hyparb_live::state_dir(&eb.path),
        budget_floor: floor,
    };
    cli::hyparb_live::boot_hyparb_live(&spec, TlsTransport::default_client_config())
}

/// **E6 commit 3/4 — the halt machine, wired.** Shared by both
/// `--exec` arms (the real Hyperliquid arm and the refusing stub), so
/// the halt file, the boot read-back and `--halt-slot` behave the same
/// whatever sits behind the router.
///
/// `set_halt_path` before any halt can fire: the writer is a no-op
/// without it, and a halt that leaves no file is cleared by the 00:10Z
/// restart and resumes trading into whatever tripped it, unattended.
fn wire_exec_halts<L: clob_dispatcher::OrderDispatch>(
    exec_dispatcher: &mut exec_router::RoutedDispatcher<clob_dispatcher::PaperDispatcher, L>,
    eb: &cli::exec_boot::ExecBoot,
    halt_mask: u8,
) {
    exec_dispatcher.set_halt_path(cli::exec_boot::halt_file_path(&eb.path));
    // **E6 commit 4 — read back what the last run halted.** Before
    // anything else touches the table: a slot the last run stopped
    // must not quote once while the boot is still talking.
    let adopted = exec_dispatcher.adopt_halt_file();
    if adopted == 0 && exec_dispatcher.halt_file_present() {
        // **A halt file that halted nothing.** A mistyped slot, a slot
        // that is not live, a line this binary could not parse — or,
        // since the E7 review, a file that exists and could not be
        // READ (permissions, over 512 B, not a regular file). Silence
        // here would be the inverse of the failure below and strictly
        // worse: an operator who asked for a halt, got a clean boot
        // log, and an engine that trades.
        error!(
            file = %cli::exec_boot::halt_file_path(&eb.path).display(),
            "exec: exec.HALT IS PRESENT BUT HALTED NOTHING — check that it is a plain \
             file under 512 B this process can read, that the slot numbers are right \
             (a line is `slot=<n> reason=<word>`, or a bare number) and that those \
             slots are LIVE. The engine is trading."
        );
    }
    if adopted > 0 {
        // Deliberately loud, and deliberately an error rather than a
        // warning. The failure this guards against is an operator
        // reading a clean boot log, assuming the halt cleared, and
        // waiting for quotes that are never coming.
        error!(
            slots = adopted,
            file = %cli::exec_boot::halt_file_path(&eb.path).display(),
            "exec: STARTED HALTED — a previous run left exec.HALT and those slots will \
             refuse every order. Investigate the recorded reason, then DELETE the file \
             and restart to clear."
        );
        for slot in 0..exec_router::EXEC_SLOTS {
            let why = exec_dispatcher.halt().reason(slot);
            if why.is_halted() {
                error!(slot, reason = why.as_str(), "exec: slot halted");
            }
        }
    }
    if halt_mask != 0 {
        // Live slots only — `halt_slot` refuses the rest, and says so
        // here rather than claiming a paper slot will refuse orders it
        // never routes through the latch.
        let mut halted = 0u8;
        for slot in 0..exec_router::EXEC_SLOTS {
            if halt_mask & (1u8 << slot) == 0 {
                continue;
            }
            if exec_dispatcher.halt_slot(slot, exec_router::HaltReason::Operator) {
                halted |= 1u8 << slot;
            } else {
                warn!(slot, "--halt-slot: slot is not LIVE — nothing to halt, flag ignored for it");
            }
        }
        if halted != 0 {
            warn!(
                halted = %cli::exec_boot::render_slot_mask(halted),
                "--halt-slot: booting with LIVE slots already HALTED — they will refuse \
                 every submit and modify until restarted without the flag"
            );
        }
    }
}

fn boot_live_dispatcher(
    cfg: &Config,
    tls_config: std::sync::Arc<rustls::ClientConfig>,
) -> Result<LiveDispatcher, &'static str> {
    // Surface the actual ConfigError variant instead of a one-size
    // "missing key?" string. Each branch logs the precise cause so
    // an operator can fix the right thing.
    let secrets = match Secrets::load() {
        Ok(s) => s,
        Err(core_config::ConfigError::Missing(k)) => {
            error!(key = k, "Secrets::load: required env var missing");
            return Err("Secrets::load: required env var missing");
        }
        Err(core_config::ConfigError::Invalid(k)) => {
            error!(
                key = k,
                "Secrets::load: env var present but not parseable (hex length / nibble)"
            );
            return Err("Secrets::load: env var unparseable");
        }
        Err(core_config::ConfigError::Mlock(errno)) => {
            error!(
                errno,
                "Secrets::load: mlock failed — raise RLIMIT_MEMLOCK or run with CAP_IPC_LOCK"
            );
            return Err("Secrets::load: mlock failed");
        }
        Err(core_config::ConfigError::DotenvMissing(path)) => {
            error!(path, "Secrets::load: .env file not found / unreadable");
            return Err("Secrets::load: .env file not found");
        }
    };
    let mut key = [0u8; 32];
    key.copy_from_slice(secrets.signing_key());
    let port = 443u16;
    LiveDispatcher::connect(&cfg.polymarket_clob_host, "/order", port, key, tls_config).map_err(
        |e| {
            error!(error = ?e, "LiveDispatcher::connect failed");
            "LiveDispatcher::connect failed (DNS / cert / key)"
        },
    )
}

/// Boot a [`LiveDispatcher`] wrapped in a [`QueuedDispatcher`].
/// Spawns the worker thread; returns the producer-side handle and
/// the worker's `JoinHandle` so the caller can keep it alive
/// until shutdown.
///
/// **Why queue?** `LiveDispatcher::submit` blocks for one TCP+TLS
/// POST round-trip (~50–100 ms over WAN). The queued path lets
/// the engine fire-and-forget — submits become a single SPSC
/// ring push (~ns) instead of a network round-trip.
fn boot_queued_live(
    cfg: &Config,
    tls_config: std::sync::Arc<rustls::ClientConfig>,
) -> Result<
    (
        clob_dispatcher::QueuedDispatcher,
        std::thread::JoinHandle<()>,
    ),
    &'static str,
> {
    let live = boot_live_dispatcher(cfg, tls_config)?;
    let (queued, worker) = clob_dispatcher::QueuedDispatcher::new(live);
    let stop_ref: &'static AtomicBool = &SHUTDOWN;
    let handle = std::thread::Builder::new()
        .name("clob-dispatcher".into())
        .spawn(move || {
            info!("clob-dispatcher worker thread up");
            worker.run(stop_ref);
            info!("clob-dispatcher worker thread exiting");
        })
        .map_err(|_| "spawn clob-dispatcher worker failed")?;
    Ok((queued, handle))
}

fn run(args: RunArgs) -> ExitCode {
    let cfg = match Config::load(args.env_file.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            error!(error = ?e, "config load failed");
            return ExitCode::from(1);
        }
    };

    info!(
        paper = args.paper,
        polymarket = %cfg.polymarket_clob_host,
        binance = %cfg.binance_ws_host,
        alchemy = %cfg.alchemy_host,
        "starting engine"
    );

    if let Err(e) = install_sigint_handler() {
        error!(error = ?e, "SIGINT handler install failed");
        return ExitCode::from(1);
    }

    let tls_config = TlsTransport::default_client_config();

    // -- M1 universe resolution (config file + flag overrides) --
    // Pure precedence law in cli::universe_boot; any failure is a
    // fatal boot error BEFORE side effects (no capture dir, no
    // discovery, no threads).
    let config_src = match cli::universe_boot::read_universe_source(args.universe.as_deref()) {
        Ok(v) => v,
        Err(reason) => {
            error!(reason, "universe config read failed");
            return ExitCode::from(1);
        }
    };
    let boot = match cli::universe_boot::resolve_boot_universe(&cli::universe_boot::UniverseFlags {
        config_src: config_src.as_deref(),
        pm_asset_id: args.polymarket_asset_id.as_deref(),
        pm_sym_id: args.polymarket_sym_id,
        bn_symbol: args.binance_symbol.as_deref(),
        bn_sym_id: args.binance_sym_id,
        okx_symbols: args.okx_symbols.as_deref(),
        deribit_symbols: args.deribit_symbols.as_deref(),
        hl_coins: args.hl_coins.as_deref(),
        okx_depth: args.okx_depth,
        deribit_depth: args.deribit_depth,
    }) {
        Ok(b) => b,
        Err(reason) => {
            error!(reason, "universe resolution failed");
            return ExitCode::from(1);
        }
    };
    info!(
        from_config = boot.from_config,
        pm_tokens = boot.allocated.pm_tokens.len(),
        bn_spot = boot.allocated.bn_spot.len(),
        bn_usdm = boot.allocated.bn_usdm.len(),
        bn_dated = boot.allocated.bn_dated.len(),
        pairs = boot.allocated.pairs.len(),
        "universe resolved"
    );
    let pm_ids: Vec<String> = boot
        .allocated
        .pm_tokens
        .iter()
        .map(|t| t.token_id.clone())
        .collect();

    // -- Raw-tap flags (Phase 8e §6.5; fail-fast on a bad spec) --
    let raw_tap_cfg = match cli::parse_raw_tap_flags(
        args.raw_tap.as_deref(),
        &args.raw_tap_mode,
        args.raw_tap_budget_mb,
    ) {
        Ok(c) => c,
        Err(reason) => {
            error!(reason, "bad --raw-tap flags");
            return ExitCode::from(1);
        }
    };
    // VT2: per-venue staleness thresholds (defaults + overrides).
    let stale_after_ms = match cli::parse_stale_after_ms(&args.stale_after_ms) {
        Ok(t) => t,
        Err(reason) => {
            error!(reason, "bad --stale-after-ms flags");
            return ExitCode::from(1);
        }
    };

    // -- Capture run directory (Phase 8e §6.5) --
    // Every spawned ingress's PmlrCapture files land under here; the
    // directory is created now so the first `PmlrCapture::open` below
    // never races its own `create_dir_all`.
    let (run_dir, epoch_ns) = match cli::new_capture_run_dir(&cfg.log_dir) {
        Ok(v) => v,
        Err(e) => {
            error!(error = ?e, dir = %cfg.log_dir, "capture: run directory create failed");
            return ExitCode::from(1);
        }
    };
    info!(dir = %run_dir.display(), "capture: run directory");

    // -- Phase-8e boot REST discovery (plan §6.1) --
    // Runs BEFORE any ingress thread spawns: validates every
    // configured symbol against the venue's live universe and (OKX
    // only) builds the discovery-gated symbol table. Any fetch/parse
    // failure is a fatal boot error in both paper and live mode — a
    // venue whose REST is down now would fail its WS subscribe anyway.
    // M1: config-driven boots get the Binance exchangeInfo audit;
    // legacy flag boots keep their historical zero-REST BN behavior.
    let bn_spot_names: Vec<String> = boot
        .allocated
        .bn_spot
        .iter()
        .map(|i| i.name.clone())
        .collect();
    let bn_usdm_names: Vec<String> = boot
        .allocated
        .bn_usdm
        .iter()
        .map(|i| i.name.clone())
        .collect();
    let bn_dated_names: Vec<String> = boot
        .allocated
        .bn_dated
        .iter()
        .map(|i| i.name.clone())
        .collect();
    let bn_discovery_arg: Option<(&[String], &[String], &[String])> = if boot.from_config {
        Some((&bn_spot_names, &bn_usdm_names, &bn_dated_names))
    } else {
        None
    };
    // M2.1: an explicit --deribit-symbols override replaced the whole
    // [deribit] section — an enabled config options policy was dropped
    // with it (M1a override law). Say so loudly.
    if boot.deribit_options_dropped {
        warn!(
            "--deribit-symbols override active — the universe config's deribit options \
             policy is DROPPED for this boot (flag replaces the venue section)"
        );
    }
    if boot.okx_options_dropped {
        warn!(
            "--okx-symbols override active — the universe config's okx options \
             policy is DROPPED for this boot (flag replaces the venue section)"
        );
    }
    if boot.bn_options_dropped {
        warn!(
            "--binance-symbol override active — the universe config's binance options \
             policy is DROPPED for this boot (flag replaces the venue section)"
        );
    }
    // WS9: the Bybit audit runs whenever the section is configured
    // (both boot classes come from the universe file only — no
    // legacy flag lane for the sixth venue).
    let bybit_spot_names: Vec<String> = boot
        .allocated
        .bybit_spot
        .iter()
        .map(|i| i.name.clone())
        .collect();
    let bybit_linear_names: Vec<String> = boot
        .allocated
        .bybit_linear
        .iter()
        .map(|i| i.name.clone())
        .collect();
    let bybit_discovery_arg: Option<(&[String], &[String])> =
        if !bybit_spot_names.is_empty() || !bybit_linear_names.is_empty() {
            Some((&bybit_spot_names, &bybit_linear_names))
        } else {
            None
        };
    // MX6: the MEXC audit + funding seeds run whenever `[mexc]` is
    // configured (config-file only, like Bybit).
    let mexc_spot_names: Vec<String> = boot
        .allocated
        .mexc_spot
        .iter()
        .map(|i| i.name.clone())
        .collect();
    let mexc_perp_names: Vec<String> = boot
        .allocated
        .mexc_perp
        .iter()
        .map(|i| i.name.clone())
        .collect();
    let mexc_discovery_arg: Option<(&[String], &[String])> =
        if !mexc_spot_names.is_empty() || !mexc_perp_names.is_empty() {
            Some((&mexc_spot_names, &mexc_perp_names))
        } else {
            None
        };
    let discovery = match cli::boot_discovery::run_all(
        &cfg,
        &tls_config,
        boot.okx_spec.as_deref(),
        &boot.okx_options,
        boot.deribit_spec.as_deref(),
        &boot.deribit_options,
        boot.hl_spec.as_deref(),
        bn_discovery_arg,
        &boot.bn_options,
        bybit_discovery_arg,
        mexc_discovery_arg,
        &boot.hypercall_options,
        &pm_ids,
    ) {
        Ok(o) => o,
        Err(reason) => {
            error!(reason, "boot discovery failed");
            return ExitCode::from(1);
        }
    };
    if discovery.any_missing {
        if args.live {
            error!(
                "discovery: configured symbol(s) missing from venue universe — refusing to start live"
            );
            return ExitCode::from(1);
        }
        warn!(
            "discovery: configured symbol(s) missing from venue universe — continuing in paper mode"
        );
    }

    // -- M2-close: options manifest (per-run sym → instrument-name
    // sidecar; docs/wire-format.md "Capture files"). Options ordinals
    // reshuffle per boot by design — offline venue+descriptor
    // consumers (§9.8 IV digest, M4 shadow-P&L) resolve through this
    // file. Written only when the boot selected ≥ 1 option; a write
    // failure is fatal like the run-dir create above (a boot that
    // cannot write its own capture dir must not trade on it).
    let options_manifest = cli::options_manifest::render(
        &discovery.deribit_options,
        &discovery.okx_options,
        &discovery.bn_options,
        &discovery.hypercall_options,
    );
    if !options_manifest.is_empty() {
        let manifest_path = run_dir.join(cli::options_manifest::OPTIONS_MANIFEST_FILE);
        if let Err(e) = std::fs::write(&manifest_path, &options_manifest) {
            error!(error = ?e, path = %manifest_path.display(), "capture: options-manifest write failed");
            return ExitCode::from(1);
        }
        info!(
            path = %manifest_path.display(),
            rows = discovery.deribit_options.len()
                + discovery.okx_options.len()
                + discovery.bn_options.len()
                + discovery.hypercall_options.len(),
            "capture: options manifest written"
        );
    }
    // M4.2 (D3): the FULL instrument manifest — every allocated
    // instrument, final §9.4 descriptors, written on EVERY boot (the
    // options file above stays one release for pre-D3 readers).
    let instrument_manifest = cli::options_manifest::render_instruments(
        &boot.allocated,
        &discovery.deribit_options,
        &discovery.okx_options,
        &discovery.bn_options,
        &discovery.hypercall_options,
    );
    {
        let manifest_path = run_dir.join(cli::options_manifest::INSTRUMENT_MANIFEST_FILE);
        if let Err(e) = std::fs::write(&manifest_path, &instrument_manifest) {
            error!(error = ?e, path = %manifest_path.display(), "capture: instrument-manifest write failed");
            return ExitCode::from(1);
        }
        info!(
            path = %manifest_path.display(),
            rows = instrument_manifest.lines().count(),
            "capture: instrument manifest written"
        );
    }

    // -- Resolve endpoints --
    // Path fixed 2026-08-14 (8d live test): the real-time host serves the
    // market channel at `/ws/market`; `/ws/` returns HTTP 404.
    let pm_ep = match WssEndpoint::resolve(&cfg.polymarket_clob_host, 443, "/ws/market") {
        Ok(e) => e,
        Err(e) => {
            error!(error = ?e, "polymarket DNS failed");
            return ExitCode::from(1);
        }
    };
    // (Binance endpoint resolution happens at spawn below — the M1
    // multi lane resolves per slot inside its thread.)

    // -- OKX boot config (Phase 8b; venue is opt-in) --
    // Host (+ optional :port) is now `cfg.okx_ws_host` (Phase 8e §9 —
    // closes the 8b deferral); the WS path stays a cli const. The
    // symbol table comes straight from `discovery.okx_table` — it's
    // already discovery-gated (built above).
    const OKX_WS_PATH: &str = "/ws/v5/public";
    let okx_boot = match discovery.okx_table {
        Some(symbols) => {
            let (okx_host, okx_port) = match cli::split_host_port(&cfg.okx_ws_host, 8443) {
                Ok(v) => v,
                Err(reason) => {
                    error!(reason, host = %cfg.okx_ws_host, "bad okx_ws_host");
                    return ExitCode::from(1);
                }
            };
            let okx_ep = match WssEndpoint::resolve(okx_host, okx_port, OKX_WS_PATH) {
                Ok(e) => e,
                Err(e) => {
                    error!(error = ?e, "okx DNS failed");
                    return ExitCode::from(1);
                }
            };
            Some((symbols, okx_ep))
        }
        None => None,
    };

    // -- Deribit boot config (Phase 8c; venue is opt-in) --
    // Host is now `cfg.deribit_ws_host` (closes the 8c deferral); WS
    // path stays a cli const. Symbol table building is unaffected by
    // discovery (unlike OKX it needs no per-instrument `instType`).
    const DERIBIT_WS_PATH: &str = "/ws/api/v2";
    // M2.1/WS6: the venue boots when static instruments are
    // configured OR the discovered options chain is non-empty OR
    // combos are configured (each alone is a valid universe).
    let deribit_spec_trim = boot
        .deribit_spec
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if boot.deribit_combos_dropped {
        warn!(
            "--deribit-symbols override active — the universe config's [deribit] combos \
             list is DROPPED for this boot (flag replaces the venue section)"
        );
    }
    let deribit_boot = if deribit_spec_trim.is_some()
        || !discovery.deribit_options.is_empty()
        || !boot.allocated.deribit_combos.is_empty()
    {
        let mut symbols = match deribit_spec_trim {
            Some(spec) => match cli::build_deribit_symbol_table(spec) {
                Ok(t) => t,
                Err(reason) => {
                    error!(reason, spec, "bad --deribit-symbols");
                    return ExitCode::from(1);
                }
            },
            None => ingress_deribit::DeribitSymbolTable::new(),
        };
        // Discovered capped chain appends AFTER the static block
        // (quote + ticker rows; ordinals already allocated in
        // discovery).
        if let Err(reason) =
            cli::extend_deribit_table_with_options(&mut symbols, &discovery.deribit_options)
        {
            error!(
                reason,
                selected = discovery.deribit_options.len(),
                "deribit options chain table build failed"
            );
            return ExitCode::from(1);
        }
        // WS6: configured combos append LAST (quote-only rows; own
        // ordinal block from allocation). No REST validation — the
        // subscribe echo is the validator (a misspelled combo never
        // echoes ⇒ boot fail-fast at first verification).
        let combo_pairs: Vec<(String, core_types::SymbolId)> = boot
            .allocated
            .deribit_combos
            .iter()
            .map(|i| (i.name.clone(), i.sym))
            .collect();
        if let Err(reason) = cli::extend_deribit_table_with_combos(&mut symbols, &combo_pairs) {
            error!(
                reason,
                combos = combo_pairs.len(),
                "deribit combos table build failed"
            );
            return ExitCode::from(1);
        }
        if !combo_pairs.is_empty() {
            info!(
                combos = combo_pairs.len(),
                "deribit: option combos wired (quote-only BBO capture; WS6)"
            );
        }
        let (deribit_host, deribit_port) = match cli::split_host_port(&cfg.deribit_ws_host, 443) {
            Ok(v) => v,
            Err(reason) => {
                error!(reason, host = %cfg.deribit_ws_host, "bad deribit_ws_host");
                return ExitCode::from(1);
            }
        };
        let deribit_ep = match WssEndpoint::resolve(deribit_host, deribit_port, DERIBIT_WS_PATH) {
            Ok(e) => e,
            Err(e) => {
                error!(error = ?e, "deribit DNS failed");
                return ExitCode::from(1);
            }
        };
        Some((symbols, deribit_ep))
    } else {
        None
    };

    // -- Hyperliquid boot config (Phase 8d; venue is opt-in) --
    // Host is now `cfg.hyperliquid_ws_host` (closes the 8d deferral);
    // WS path stays a cli const. Coin table building is unaffected by
    // discovery (unlike OKX it needs no per-coin metadata).
    const HL_WS_PATH: &str = "/ws";
    let hl_boot = match boot.hl_spec.as_deref().map(str::trim) {
        Some(spec) if !spec.is_empty() => {
            let mut coins = match cli::build_hl_coin_table(spec) {
                Ok(t) => t,
                Err(reason) => {
                    error!(reason, spec, "bad --hl-coins");
                    return ExitCode::from(1);
                }
            };
            // BIN15 O2: reserve the rolling families' slots AFTER the
            // configured coins, so no configured sym moves.
            let families = match cli::build_hl_families(&boot.hl_rolling, &mut coins) {
                Ok(t) => t,
                Err(reason) => {
                    error!(reason, "bad [hyperliquid] rolling");
                    return ExitCode::from(1);
                }
            };
            let (hl_host, hl_port) = match cli::split_host_port(&cfg.hyperliquid_ws_host, 443) {
                Ok(v) => v,
                Err(reason) => {
                    error!(reason, host = %cfg.hyperliquid_ws_host, "bad hyperliquid_ws_host");
                    return ExitCode::from(1);
                }
            };
            let hl_ep = match WssEndpoint::resolve(hl_host, hl_port, HL_WS_PATH) {
                Ok(e) => e,
                Err(e) => {
                    error!(error = ?e, "hyperliquid DNS failed");
                    return ExitCode::from(1);
                }
            };
            Some((coins, families, hl_ep))
        }
        _ => None,
    };

    // -- 8g §4.3: boot-universe snapshot for the ruleset validator --
    // Built ONCE here — after 8e discovery gated the venue tables,
    // before any ingress thread spawns (the tables move into their
    // spawn calls below). Sorted strict-ascending, deduped; feeds
    // `spawn_ai` → `RulesetSidePath` (§4.2 rule-6 membership checks).
    // M1: spawn-aligned — every PM token + every Binance instrument
    // (spot + USDS-M) this boot wires.
    let pm_syms: Vec<core_types::SymbolId> =
        boot.allocated.pm_tokens.iter().map(|t| t.sym).collect();
    let bn_syms: Vec<core_types::SymbolId> = boot
        .allocated
        .bn_spot
        .iter()
        .chain(boot.allocated.bn_usdm.iter())
        .chain(boot.allocated.bn_dated.iter())
        .map(|i| i.sym)
        .collect();
    // MX6 (ruling Q-MX6): every MEXC instrument this boot allocates
    // is addressable by AI rulesets (spot + perp).
    let mexc_syms: Vec<core_types::SymbolId> = boot
        .allocated
        .mexc_spot
        .iter()
        .chain(boot.allocated.mexc_perp.iter())
        .map(|i| i.sym)
        .collect();
    let ai_universe = cli::build_ai_universe(
        &pm_syms,
        &bn_syms,
        okx_boot.as_ref().map(|(t, _)| t),
        deribit_boot.as_ref().map(|(t, _)| t),
        hl_boot.as_ref().map(|(t, _f, _e)| t),
        &mexc_syms,
    );
    // VM2 V4 (D-6): the live descriptor→(sym, caps) table for the v2
    // grammar's stage-time resolution — same allocation truth as the
    // instrument manifest.
    let ai_descriptors = std::sync::Arc::new(ingress_ai::DescriptorTable::from_entries(
        cli::options_manifest::build_descriptor_entries(
            &boot.allocated,
            &discovery.deribit_options,
            &discovery.okx_options,
            &discovery.bn_options,
            &discovery.hypercall_options,
            boot.okx_depth,
            boot.deribit_depth,
        ),
    ));
    info!(
        symbols = ai_universe.len(),
        "ai: ruleset boot-universe snapshot built"
    );

    // -- Allocate rings + split into producer/consumer halves --
    //
    // Phase 8a lane layout: rings.tick is indexed by VenueId
    // (0=PM, 1=BN, 2=OKX, 3=Deribit, 4=HL). Lane 2 gains its
    // producer below when `--okx-symbols` is set (Phase 8b),
    // lane 3 when `--deribit-symbols` is set (Phase 8c) and
    // lane 4 when `--hl-coins` is set (Phase 8d); an unspawned
    // venue's producer is deliberately dropped, leaving a
    // permanently-empty ring the engine drains for two atomic
    // loads per iteration (§3.3). Fill-lane producers arrive with
    // the venue dispatchers in 8j; paper fills flow through the
    // engine's dispatcher pump (D3).
    let rings = Rings::new();
    let (pm_prod, pm_lane_cons) = rings.tick[0].clone().split();
    let (bn_prod, bn_lane_cons) = rings.tick[1].clone().split();
    let (okx_prod, okx_lane_cons) = rings.tick[2].clone().split();
    let (deribit_prod, deribit_lane_cons) = rings.tick[3].clone().split();
    let (hl_prod, hl_lane_cons) = rings.tick[4].clone().split();
    // WS9: lane 5 = Bybit (VenueId 6 — lane≠venue past Ai, see
    // engine::tick_lane_of).
    let (bybit_prod, bybit_lane_cons) = rings.tick[5].clone().split();
    // MX2: lane 6 = MEXC (VenueId 7, engine::tick_lane_of).
    let (mexc_prod, mexc_lane_cons) = rings.tick[6].clone().split();
    // HC1: lane 7 = Hypercall (VenueId 9 — HyperEvm, 8, has no tick
    // lane; engine::tick_lane_of).
    let (hypercall_prod, hypercall_lane_cons) = rings.tick[7].clone().split();
    // WS10-A: venue-event lanes, tick-lane indexing. Producers ride
    // into the four funding-capable venue spawns; PM (0) and the
    // spare lane 4 producer for HL are dropped — HL carries premium
    // inside its AssetCtx capture events and gains a lane push only
    // when a funding-bearing channel exists for it.
    let (bn_event_prod, bn_event_cons) = rings.event[1].clone().split();
    let (okx_event_prod, okx_event_cons) = rings.event[2].clone().split();
    let (deribit_event_prod, deribit_event_cons) = rings.event[3].clone().split();
    let (bybit_event_prod, bybit_event_cons) = rings.event[5].clone().split();
    let (mexc_event_prod, mexc_event_cons) = rings.event[6].clone().split();
    let (hypercall_event_prod, hypercall_event_cons) = rings.event[7].clone().split();
    let (_pm_event_prod, pm_event_cons) = rings.event[0].clone().split();
    // VM2 V2: HL gained its event lane — funding rides AssetCtx.
    let (hl_event_prod, hl_event_cons) = rings.event[4].clone().split();
    let event_lane_cons = [
        pm_event_cons,
        bn_event_cons,
        okx_event_cons,
        deribit_event_cons,
        hl_event_cons,
        bybit_event_cons,
        mexc_event_cons,
        hypercall_event_cons,
    ];
    // WS10-B: depth lanes (engine::depth_lane_of order — okx 0,
    // deribit 1). Producers ride into the two depth-capable spawns.
    let (okx_depth_prod, okx_depth_cons) = rings.depth[0].clone().split();
    let (deribit_depth_prod, deribit_depth_cons) = rings.depth[1].clone().split();
    let depth_lane_cons = [okx_depth_cons, deribit_depth_cons];
    // VM2 V2: options-summary lanes (engine::opt_lane_of order —
    // okx 0, deribit 1, binance 2, hypercall 3 — HC1). Producers ride
    // into the options-capable spawns.
    let (okx_opt_prod, okx_opt_cons) = rings.opt[0].clone().split();
    let (deribit_opt_prod, deribit_opt_cons) = rings.opt[1].clone().split();
    let (bn_opt_prod, bn_opt_cons) = rings.opt[2].clone().split();
    let (hypercall_opt_prod, hypercall_opt_cons) = rings.opt[3].clone().split();
    let opt_lane_cons = [
        okx_opt_cons,
        deribit_opt_cons,
        bn_opt_cons,
        hypercall_opt_cons,
    ];
    let (rpc_prod, rpc_cons) = rings.rpc_signal.clone().split();
    let (hyperevm_prod, hyperevm_cons) = rings.hyperevm_signal.clone().split();
    // E7: lane 3 (`engine::fill_lane_of(Hyperliquid)`) finally has a
    // producer — the live arm's user-event pump. Until E7 every lane's
    // producer was dropped here, so the E6 exposure ledger and the
    // `on_fill_booked` ordering were reachable only from tests.
    let (mut hl_fill_prod, fill_lane_cons) = {
        let (_f0p, f0) = rings.fill[0].clone().split();
        let (_f1p, f1) = rings.fill[1].clone().split();
        let (_f2p, f2) = rings.fill[2].clone().split();
        let (f3p, f3) = rings.fill[3].clone().split();
        (Some(f3p), [f0, f1, f2, f3])
    };
    // AI command lane (Phase 8f). The producer half feeds the
    // `ingress-ai` thread (spawned below, gated on
    // AI_INGRESS_HMAC_KEY); when the key is absent the producer is
    // dropped and the engine's AI lane reads permanently empty — the
    // §3.3 unspawned shape.
    let (ai_prod, ai_lane_cons) = rings.ai.clone().split();
    let ai_status = std::sync::Arc::new(cli::AiIngressStatus::new());
    // Ruleset-table handoff ring (Phase 8g §6, item 7): the producer
    // half rides with the AI lane into `spawn_ai`; the consumer half
    // rides in `Consumers` to the engine, which pops it immediately
    // before the AI-cmd drain each iteration and lends each slot in
    // place to the strategy's `on_ruleset_table` hook (→ the set's vm
    // member, whose copy into its staged buffer is documented copy #2).
    // Key-unset boots drop the producer and the lane reads empty
    // forever (§3.3 unspawned shape).
    let (ruleset_table_prod, ruleset_table_cons) = rings.ruleset_tables.clone().split();

    // -- Per-ingress status slots (D7) --
    let statuses = std::sync::Arc::new(cli::IngressStatusSet::new());
    let mut handles: Vec<std::thread::JoinHandle<()>> = Vec::new();

    // -- Build observability surfaces --
    // Built here (before any ingress thread spawns) rather than after
    // — Part B.4's two §6.5 capture gauges are set from inside each
    // spawn wrapper thread itself, so the registry + gauge ids must
    // already exist at spawn time.
    // E1: the execution route table. Resolved HERE — before the
    // metric registry is built, before any socket opens — for two
    // reasons. (1) The registry is boot-only-insertion, and whether
    // the `engine_exec_*` family exists at all depends on this
    // artifact: no `--exec` registers NOTHING, which is what keeps
    // `/metrics` byte-identical to a pre-E1 binary. (2) A disagreement
    // between the two arming switches, or a live slot with no compiled
    // arm, should abort before the engine has done anything at all,
    // not after it is already streaming six venues.
    // E6: parsed HERE, beside the artifact, for the same reason — a
    // malformed kill switch must abort the boot, not be discovered at
    // the moment someone reaches for it.
    let halt_mask = match args
        .halt_slot
        .as_deref()
        .map_or(Ok(0u8), cli::exec_boot::parse_halt_slots)
    {
        Ok(m) => m,
        Err(reason) => {
            error!(reason, "exec: --halt-slot refused — boot aborted");
            join_reverse(handles);
            return ExitCode::from(1);
        }
    };
    // Said out loud rather than refused. An operator reaching for the
    // kill switch on a boot that has no route table has asked for
    // something that is already true — every slot is paper and
    // nothing can reach a venue — and refusing to start would be
    // punishing them for asking for MORE safety. Staying silent would
    // be worse: it would let them believe a switch fired that did not.
    if halt_mask != 0 && args.exec.is_none() {
        warn!(
            halted = %cli::exec_boot::render_slot_mask(halt_mask),
            "--halt-slot named slots but there is no --exec artifact: \
             every slot is already paper and no order can reach a venue, \
             so this flag halted nothing"
        );
    }
    let exec_boot = match cli::exec_boot::resolve(args.exec.as_deref(), args.arm_live.as_deref()) {
        Ok(e) => e,
        Err(reason) => {
            error!(reason, "exec: artifact refused — boot aborted");
            join_reverse(handles);
            return ExitCode::from(1);
        }
    };
    let exec_modes = exec_boot.as_ref().map(|b| {
        let mut m = [0u8; clob_dispatcher::EXEC_COUNTER_SLOTS];
        for (slot, dst) in m.iter_mut().enumerate() {
            *dst = b
                .route
                .mode_at(slot)
                .unwrap_or(exec_router::ExecMode::Paper)
                .as_u8();
        }
        m
    });
    let enable_metrics = args.metrics || args.tui;
    let obs = match Observability::build(enable_metrics, exec_modes) {
        // RG6: the `/state` boot identity (pid, anchor, binary link
        // time, git sha, run dir, `--strategy`); masks + regime hash
        // are stamped by the set arm below.
        Ok(o) => o
            .with_ingress_statuses(statuses.clone())
            // `boot.paper` feeds `/state`, the TUI and the 9292
            // dashboard, and is documented "1 = paper mode (no live
            // dispatcher)". `--exec` can arm a slot without ever
            // setting `--live`, so asking `!args.live` alone would
            // report PAPER on an engine that is routing real orders.
            .with_boot_info(boot_info(
                &run_dir,
                epoch_ns,
                &args.strategy,
                !args.live && !exec_boot.as_ref().is_some_and(cli::exec_boot::ExecBoot::any_live),
            )),
        Err(reason) => {
            error!(reason, "observability build failed");
            join_reverse(handles);
            return ExitCode::from(1);
        }
    };
    // Resolve latency-dump destination. Defaults to
    // `<cfg.log_dir>/latency` so an operator who already set
    // `MULTIVENUE_LOG_DIR` doesn't need a second flag.
    let latency_dump_dir = args
        .latency_dump_dir
        .clone()
        .unwrap_or_else(|| PathBuf::from(format!("{}/latency", cfg.log_dir)));
    let obs = obs.with_latency_dump(LatencyDump::from_secs(
        latency_dump_dir.clone(),
        args.latency_dump_secs,
    ));
    if args.latency_dump_secs > 0 {
        info!(
            secs = args.latency_dump_secs,
            dir = %latency_dump_dir.display(),
            "latency dump enabled"
        );
    }

    // §6.1 boot-discovery coverage gauges — static boot-time counts,
    // set once (not mirrored on a cadence like the §6.4 counters).
    if let (Some(reg), Some(ids)) = (obs.metrics.as_ref(), obs.counter_ids.as_ref()) {
        reg.gauge(ids.coverage_pm)
            .set(discovery.pm.configured as i64);
        reg.gauge(ids.coverage_okx)
            .set(discovery.okx.map(|c| c.configured).unwrap_or(0) as i64);
        reg.gauge(ids.coverage_deribit)
            .set(discovery.deribit.map(|c| c.configured).unwrap_or(0) as i64);
        reg.gauge(ids.coverage_hyperliquid)
            .set(discovery.hl.map(|c| c.configured).unwrap_or(0) as i64);
        reg.gauge(ids.coverage_binance)
            .set(discovery.bn.map(|c| c.configured).unwrap_or(0) as i64);
        reg.gauge(ids.coverage_bybit)
            .set(discovery.bybit.map(|c| c.configured).unwrap_or(0) as i64);
        reg.gauge(ids.coverage_mexc)
            .set(discovery.mexc.map(|c| c.configured).unwrap_or(0) as i64);
        reg.gauge(ids.coverage_hypercall)
            .set(discovery.hypercall.map(|c| c.configured).unwrap_or(0) as i64);
        // M2.1/M2.2/M2.4: capped options chain sizes this boot
        // (0 = lane off).
        reg.gauge(ids.deribit_options_selected)
            .set(discovery.deribit_options.len() as i64);
        reg.gauge(ids.okx_options_selected)
            .set(discovery.okx_options.len() as i64);
        reg.gauge(ids.binance_options_selected)
            .set(discovery.bn_options.len() as i64);
        reg.gauge(ids.hypercall_options_selected)
            .set(discovery.hypercall_options.len() as i64);
    }

    // Per-venue (registry, gauge-ids) pair for the §6.5 capture
    // metrics — `None` end-to-end when `--metrics`/`--tui` are off.
    let capture_metrics_for = |ids: Option<cli::CaptureGaugeIds>| -> cli::CaptureMetrics {
        match (obs.metrics.as_ref(), ids) {
            (Some(reg), Some(ids)) => Some((reg.clone(), ids)),
            _ => None,
        }
    };

    // -- Spawn ingress threads --

    // D1 fix + M1 multi-market: the PM symbol map carries EVERY
    // configured token id → sym pair. Venue-wide REST discovery
    // (above) validated each against the live venue.
    let pm_map = ingress_polymarket::run_loop::SymbolMap::from_pairs(
        boot.allocated
            .pm_tokens
            .iter()
            .map(|t| (t.token_id.clone().into_bytes(), t.sym)),
    );
    info!(
        markets = boot.allocated.pm_tokens.len(),
        first_sym = boot.allocated.pm_tokens[0].sym,
        "polymarket: symbol map configured"
    );
    let pm_id_bytes: Vec<Vec<u8>> = boot
        .allocated
        .pm_tokens
        .iter()
        .map(|t| t.token_id.clone().into_bytes())
        .collect();
    let pm_handle = match spawn_polymarket(
        pm_ep,
        tls_config.clone(),
        pm_map,
        pm_id_bytes,
        stale_after_ms[core_types::VenueId::Polymarket as usize],
        pm_prod,
        statuses.polymarket.clone(),
        1,
        &run_dir,
        epoch_ns,
        raw_tap_cfg.pm,
        capture_metrics_for(obs.counter_ids.as_ref().map(|c| c.capture_pm)),
    ) {
        Ok(h) => h,
        Err(e) => {
            error!(error = ?e, "polymarket: capture open failed");
            join_reverse(handles);
            return ExitCode::from(1);
        }
    };
    handles.push(pm_handle);

    // Binance: the legacy single-stream lane for one-symbol boots
    // (byte-identical pre-M1, soak-proven), the M1 multi-connection
    // lane whenever the universe wires more than one BN instrument —
    // or (M2.4) whenever the eapi options chain is on: the eapi slot
    // is a MultiConn slot, keeping the venue single-writer.
    // WS5: dated futures ride the fapi host beside the perps (their
    // own ordinal block); every USDS-M instrument (perp + dated) also
    // gets a `@markPrice` slot — the capture-only mark/index/funding
    // lane (dated frames carry no funding; the parser's has_funding
    // gate handles it).
    let bn_usdm_all = boot.allocated.bn_usdm.len() + boot.allocated.bn_dated.len();
    let bn_total = boot.allocated.bn_spot.len() + bn_usdm_all;
    let bn_eapi_on = !discovery.bn_options.is_empty();
    let bn_handle = if bn_total > 1 || bn_usdm_all > 0 || bn_eapi_on {
        let mut specs: Vec<cli::BinanceConnSpec> =
            Vec::with_capacity(bn_total + bn_usdm_all + usize::from(bn_eapi_on));
        for inst in &boot.allocated.bn_spot {
            specs.push(cli::BinanceConnSpec {
                host: cfg.binance_ws_host.clone(),
                path: format!("/ws/{}@bookTicker", inst.name),
                sym: inst.sym,
                eapi: None,
                mark_price: false,
                // VT2: spot bookTicker carries no venue stamp — the
                // aggTrade sentinel rides the same socket.
                spot_sentinel: true,
            });
        }
        for inst in boot
            .allocated
            .bn_usdm
            .iter()
            .chain(boot.allocated.bn_dated.iter())
        {
            // BX0-F1: bookTicker on the legacy `/ws/` path (still
            // delivering), markPrice on the routed `/market/ws/` path —
            // one builder shared with the live smoke.
            let [book, mark] = cli::bn_usdm_specs(&cfg.binance_fut_ws_host, &inst.name, inst.sym);
            specs.push(book);
            specs.push(mark);
        }
        if bn_eapi_on {
            // M2.4 / BX0-F2: ONE combined slot, one
            // `<uly>@optionMarkPrice` stream per underlying on
            // fstream's routed `/market` path; each push is the whole
            // chain of that underlying and the table keeps the
            // selected rows (no subscribe frames — the house
            // direct-URL pattern).
            let mut table = ingress_binance::eapi::EapiSymbolTable::new();
            for (symbol, sym) in &discovery.bn_options {
                if let Err(e) = table.insert(symbol.as_bytes(), *sym) {
                    error!(?e, symbol = %symbol, "binance: options table build failed");
                    join_reverse(handles);
                    return ExitCode::from(1);
                }
            }
            let path = cli::bn_options_path(&boot.bn_options.underlyings);
            // Loud endpoint provenance — the options WS base has
            // churned (nbstream `/eoptions` retired, fstream `/market`
            // since the 2025-12 migration); a host pinned in `.env`
            // shows here, and so does its 404.
            info!(
                host = %cfg.binance_eapi_ws_host,
                path = %path,
                underlyings = boot.bn_options.underlyings.len(),
                selected = discovery.bn_options.len(),
                "binance: options mark-array slot (override via BINANCE_EAPI_WS_HOST)"
            );
            specs.push(cli::BinanceConnSpec {
                host: cfg.binance_eapi_ws_host.clone(),
                path,
                sym: 0,
                eapi: Some(table),
                mark_price: false,
                spot_sentinel: false,
            });
        }
        info!(
            conns = specs.len(),
            spot = boot.allocated.bn_spot.len(),
            usdm = boot.allocated.bn_usdm.len(),
            dated = boot.allocated.bn_dated.len(),
            mark_price = bn_usdm_all,
            eapi_options = discovery.bn_options.len(),
            stale_after_ms = stale_after_ms[core_types::VenueId::Binance as usize],
            "binance: M1 multi-connection lane"
        );
        match cli::spawn_binance_multi(
            specs,
            tls_config.clone(),
            stale_after_ms[core_types::VenueId::Binance as usize],
            bn_prod,
            bn_event_prod,
            bn_opt_prod,
            statuses.binance.clone(),
            2,
            &run_dir,
            epoch_ns,
            raw_tap_cfg.bn,
            capture_metrics_for(obs.counter_ids.as_ref().map(|c| c.capture_bn)),
        ) {
            Ok(h) => h,
            Err(e) => {
                error!(error = ?e, "binance: capture open failed");
                join_reverse(handles);
                return ExitCode::from(1);
            }
        }
    } else {
        let bn_path = format!("/ws/{}@bookTicker", boot.allocated.bn_spot[0].name);
        let bn_ep = match WssEndpoint::resolve(&cfg.binance_ws_host, 443, &bn_path) {
            Ok(e) => e,
            Err(e) => {
                error!(error = ?e, "binance DNS failed");
                join_reverse(handles);
                return ExitCode::from(1);
            }
        };
        match spawn_binance(
            bn_ep,
            tls_config.clone(),
            boot.allocated.bn_spot[0].sym,
            stale_after_ms[core_types::VenueId::Binance as usize],
            bn_prod,
            bn_event_prod,
            bn_opt_prod,
            statuses.binance.clone(),
            2,
            &run_dir,
            epoch_ns,
            raw_tap_cfg.bn,
            capture_metrics_for(obs.counter_ids.as_ref().map(|c| c.capture_bn)),
        ) {
            Ok(h) => h,
            Err(e) => {
                error!(error = ?e, "binance: capture open failed");
                join_reverse(handles);
                return ExitCode::from(1);
            }
        }
    };
    handles.push(bn_handle);

    // OKX rides core 5 per the §9 core map (rpc keeps 3, ai takes 4).
    if let Some((okx_symbols, okx_ep)) = okx_boot {
        info!(
            instruments = okx_symbols.len(),
            depth = boot.okx_depth,
            stale_after_ms = stale_after_ms[core_types::VenueId::Okx as usize],
            "okx: starting ingress thread"
        );
        let okx_handle = match spawn_okx(
            okx_ep,
            tls_config.clone(),
            okx_symbols,
            boot.okx_depth,
            // M2.3: family-keyed opt-summary subscription args.
            boot.okx_options.underlyings.clone(),
            stale_after_ms[core_types::VenueId::Okx as usize],
            okx_prod,
            okx_event_prod,
            okx_depth_prod,
            okx_opt_prod,
            statuses.okx.clone(),
            5,
            &run_dir,
            epoch_ns,
            raw_tap_cfg.okx,
            capture_metrics_for(obs.counter_ids.as_ref().map(|c| c.capture_okx)),
        ) {
            Ok(h) => h,
            Err(e) => {
                error!(error = ?e, "okx: capture open failed");
                join_reverse(handles);
                return ExitCode::from(1);
            }
        };
        handles.push(okx_handle);
    } else {
        info!("--okx-symbols empty / unset; OKX ingress thread not started");
        // Drop the producer sides so the lanes stay permanently-
        // empty rings (the unspawned-venue shape, §3.3).
        drop(okx_prod);
        drop(okx_event_prod);
        drop(okx_depth_prod);
        drop(okx_opt_prod);
    }

    // Deribit rides core 6 per the §9 core map.
    if let Some((deribit_symbols, deribit_ep)) = deribit_boot {
        // WS6: DVOL index subscriptions derive from the configured
        // options underlyings (BTC → btc_usd) — no extra config key.
        // Outside the subscribe-verification mask: an index the venue
        // does not serve simply never echoes (missing capture series,
        // never a session verdict).
        let dvol_indices: Vec<String> = boot
            .deribit_options
            .underlyings
            .iter()
            .map(|u| format!("{}_usd", u.to_ascii_lowercase()))
            .collect();
        info!(
            instruments = deribit_symbols.len(),
            depth = boot.deribit_depth,
            dvol = dvol_indices.len(),
            stale_after_ms = stale_after_ms[core_types::VenueId::Deribit as usize],
            "deribit: starting ingress thread"
        );
        let deribit_handle = match spawn_deribit(
            deribit_ep,
            tls_config.clone(),
            deribit_symbols,
            boot.deribit_depth,
            dvol_indices,
            stale_after_ms[core_types::VenueId::Deribit as usize],
            deribit_prod,
            deribit_event_prod,
            deribit_depth_prod,
            deribit_opt_prod,
            statuses.deribit.clone(),
            6,
            &run_dir,
            epoch_ns,
            raw_tap_cfg.deribit,
            capture_metrics_for(obs.counter_ids.as_ref().map(|c| c.capture_deribit)),
        ) {
            Ok(h) => h,
            Err(e) => {
                error!(error = ?e, "deribit: capture open failed");
                join_reverse(handles);
                return ExitCode::from(1);
            }
        };
        handles.push(deribit_handle);
    } else {
        info!("--deribit-symbols empty / unset; Deribit ingress thread not started");
        // Drop the producer sides so the lanes stay permanently-
        // empty rings (the unspawned-venue shape, §3.3).
        drop(deribit_prod);
        drop(deribit_event_prod);
        drop(deribit_depth_prod);
        drop(deribit_opt_prod);
    }

    // Hyperliquid rides core 7 per the §9 core map.
    let hl_wall_anchor = core_time::WallAnchor::now();
    if let Some((mut hl_coins, mut hl_families, hl_ep)) = hl_boot {
        // BIN15 O2: adopt each family's LIVE instance out of the
        // discovery body, so the first `Steady` subscribes it instead
        // of waiting up to a whole period for the venue's next
        // lifecycle push. A family with no match boots dormant.
        if !hl_families.is_empty() {
            // WALL clock: a HIP-4 expiry is an epoch instant, and
            // this anchor is the one the ingress will judge rolls
            // against for the life of the process.
            let bound = hl_families.bind_live(
                &discovery.hl_outcome_specs,
                hl_wall_anchor.wall_ns,
                &mut hl_coins,
            );
            info!(
                families = hl_families.len(),
                bound,
                dormant = hl_families.dormant_count(),
                candidates = discovery.hl_outcome_specs.len(),
                "hyperliquid: rolling families bound"
            );
            for f in 0..hl_families.len() {
                let Some(row) = hl_families.get(f) else {
                    continue;
                };
                let underlying = core::str::from_utf8(row.underlying_bytes()).unwrap_or("?");
                if row.dormant {
                    info!(
                        family = f,
                        underlying,
                        period_s = row.period_s,
                        sym_yes = row.sym[0],
                        sym_no = row.sym[1],
                        "hyperliquid: family dormant (no live instance)"
                    );
                } else {
                    info!(
                        family = f,
                        underlying,
                        period_s = row.period_s,
                        sym_yes = row.sym[0],
                        sym_no = row.sym[1],
                        live = row.live.outcome,
                        strike_1e6 = row.live.strike_1e6,
                        expiry_ns = row.live.expiry_ns,
                        twap_s = row.live.twap_s,
                        "hyperliquid: family live"
                    );
                }
            }
        }
        info!(
            coins = hl_coins.len(),
            families = hl_families.len(),
            stale_after_ms = stale_after_ms[core_types::VenueId::Hyperliquid as usize],
            "hyperliquid: starting ingress thread"
        );
        let hl_handle = match spawn_hyperliquid(
            hl_ep,
            tls_config.clone(),
            hl_coins,
            hl_families,
            statuses.hl_roll.clone(),
            hl_wall_anchor,
            stale_after_ms[core_types::VenueId::Hyperliquid as usize],
            hl_prod,
            hl_event_prod,
            statuses.hyperliquid.clone(),
            7,
            &run_dir,
            epoch_ns,
            raw_tap_cfg.hl,
            capture_metrics_for(obs.counter_ids.as_ref().map(|c| c.capture_hyperliquid)),
        ) {
            Ok(h) => h,
            Err(e) => {
                error!(error = ?e, "hyperliquid: capture open failed");
                join_reverse(handles);
                return ExitCode::from(1);
            }
        };
        handles.push(hl_handle);
    } else {
        info!("--hl-coins empty / unset; Hyperliquid ingress thread not started");
        // Drop the producer side so the lane stays a permanently-
        // empty ring (the unspawned-venue shape, §3.3).
        drop(hl_prod);
        drop(hl_event_prod);
    }

    // WS9: Bybit — spot + linear connection slots on ONE thread
    // (core 8 per the §9 core-map extension). Config-file only.
    if !boot.allocated.bybit_spot.is_empty() || !boot.allocated.bybit_linear.is_empty() {
        let mut specs: Vec<cli::BybitConnSpec> = Vec::new();
        if !boot.allocated.bybit_spot.is_empty() {
            let mut table = ingress_bybit::BybitSymbolTable::new();
            for inst in &boot.allocated.bybit_spot {
                if let Err(e) = table.insert(inst.name.as_bytes(), inst.sym) {
                    error!(?e, symbol = %inst.name, "bybit: spot table build failed");
                    join_reverse(handles);
                    return ExitCode::from(1);
                }
            }
            specs.push(cli::BybitConnSpec {
                path: "/v5/public/spot".to_string(),
                table,
                want_tickers: false,
            });
        }
        if !boot.allocated.bybit_linear.is_empty() {
            let mut table = ingress_bybit::BybitSymbolTable::new();
            for inst in &boot.allocated.bybit_linear {
                if let Err(e) = table.insert(inst.name.as_bytes(), inst.sym) {
                    error!(?e, symbol = %inst.name, "bybit: linear table build failed");
                    join_reverse(handles);
                    return ExitCode::from(1);
                }
            }
            specs.push(cli::BybitConnSpec {
                path: "/v5/public/linear".to_string(),
                table,
                want_tickers: true,
            });
        }
        info!(
            spot = boot.allocated.bybit_spot.len(),
            linear = boot.allocated.bybit_linear.len(),
            conns = specs.len(),
            stale_after_ms = stale_after_ms[core_types::VenueId::Bybit as usize],
            "bybit: starting ingress thread"
        );
        let bybit_handle = match cli::spawn_bybit(
            cfg.bybit_ws_host.clone(),
            specs,
            tls_config.clone(),
            stale_after_ms[core_types::VenueId::Bybit as usize],
            bybit_prod,
            bybit_event_prod,
            statuses.bybit.clone(),
            8,
            &run_dir,
            epoch_ns,
            raw_tap_cfg.bybit,
            capture_metrics_for(obs.counter_ids.as_ref().map(|c| c.capture_bybit)),
        ) {
            Ok(h) => h,
            Err(e) => {
                error!(error = ?e, "bybit: capture open failed");
                join_reverse(handles);
                return ExitCode::from(1);
            }
        };
        handles.push(bybit_handle);
    } else {
        // Drop the producer sides so the lanes stay permanently-
        // empty rings (the unspawned-venue shape, §3.3).
        drop(bybit_prod);
        drop(bybit_event_prod);
    }

    // MX6: MEXC — spot (protobuf, `/ws`) + futures (JSON, `/edge`)
    // connection slots on ONE thread (core 9, the §9 core-map
    // extension past Bybit's 8). Config-file only. Each class is
    // chunked to its MEASURED per-socket cap (plan §4 D1: spot 30 subs
    // = 15 symbols × 2 channels, futures 13 symbols × 3 channels);
    // futures conns carry the boot REST funding seeds (Q-MX3).
    if !boot.allocated.mexc_spot.is_empty() || !boot.allocated.mexc_perp.is_empty() {
        let mut specs: Vec<cli::MexcConnSpec> = Vec::new();
        for (class, insts, host) in [
            (
                ingress_mexc::MexcClass::Spot,
                &boot.allocated.mexc_spot,
                &cfg.mexc_ws_host,
            ),
            (
                ingress_mexc::MexcClass::Futures,
                &boot.allocated.mexc_perp,
                &cfg.mexc_fut_ws_host,
            ),
        ] {
            let Ok(path) = std::str::from_utf8(class.ws_path()) else {
                error!(class = class.label(), "mexc: non-utf8 ws path");
                join_reverse(handles);
                return ExitCode::from(1);
            };
            for chunk in insts.chunks(class.symbols_per_conn()) {
                let mut table = ingress_mexc::MexcSymbolTable::new();
                let mut funding_seeds = Vec::new();
                for inst in chunk {
                    if let Err(e) = table.insert(inst.name.as_bytes(), inst.sym) {
                        error!(?e, symbol = %inst.name, class = class.label(), "mexc: table build failed");
                        join_reverse(handles);
                        return ExitCode::from(1);
                    }
                    if class == ingress_mexc::MexcClass::Futures {
                        match discovery.mexc_funding.iter().find(|(n, _)| *n == inst.name) {
                            Some((_, seed)) => funding_seeds.push((
                                inst.sym,
                                seed.next_settle_ms,
                                seed.collect_cycle_h,
                            )),
                            // Missing from the venue (discovery already
                            // flagged it): no seed, Funding v1 stays 0.
                            None => warn!(symbol = %inst.name, "mexc: no funding seed — Funding v1 = 0"),
                        }
                    }
                }
                specs.push(cli::MexcConnSpec {
                    class,
                    host: host.clone(),
                    path: path.to_string(),
                    table,
                    funding_seeds,
                });
            }
        }
        info!(
            spot = boot.allocated.mexc_spot.len(),
            perp = boot.allocated.mexc_perp.len(),
            conns = specs.len(),
            seeds = discovery.mexc_funding.len(),
            stale_after_ms = stale_after_ms[core_types::VenueId::Mexc as usize],
            "mexc: starting ingress thread"
        );
        let mexc_handle = match cli::spawn_mexc(
            specs,
            tls_config.clone(),
            stale_after_ms[core_types::VenueId::Mexc as usize],
            mexc_prod,
            mexc_event_prod,
            statuses.mexc.clone(),
            9,
            &run_dir,
            epoch_ns,
            raw_tap_cfg.mexc,
            capture_metrics_for(obs.counter_ids.as_ref().map(|c| c.capture_mexc)),
        ) {
            Ok(h) => h,
            Err(e) => {
                error!(error = ?e, "mexc: capture open failed");
                join_reverse(handles);
                return ExitCode::from(1);
            }
        };
        handles.push(mexc_handle);
    } else {
        // Drop the producer sides so the lanes stay permanently-
        // empty rings (the unspawned-venue shape, §3.3) — a boot with
        // no `[mexc]` section is the pre-MEXC boot, bit for bit.
        drop(mexc_prod);
        drop(mexc_event_prod);
    }

    // -- Hypercall (HC5; data-only, ruling O-HC1): ONE public socket on
    // its own thread (core 11, past HyperEVM's 10) + the REST poller
    // thread, whenever `[hypercall]` selected a chain at boot. The
    // universe is the HC4 discovery outcome; the index syms are the
    // config file's. --
    if !discovery.hypercall_options.is_empty() {
        let mut symbols = ingress_hypercall::HcSymbolTable::new();
        for (name, sym, ..) in &discovery.hypercall_options {
            if let Err(e) = symbols.insert(name.as_bytes(), *sym) {
                error!(?e, instrument = %name, "hypercall: table build failed");
                join_reverse(handles);
                return ExitCode::from(1);
            }
        }
        let mut underlyings = ingress_hypercall::HcUnderlyings::new();
        for inst in &boot.allocated.hypercall_idx {
            if let Err(e) = underlyings.insert(inst.name.as_bytes(), inst.sym) {
                error!(?e, underlying = %inst.name, "hypercall: index table build failed");
                join_reverse(handles);
                return ExitCode::from(1);
            }
        }
        let stale = stale_after_ms[core_types::VenueId::Hypercall as usize];
        info!(
            instruments = discovery.hypercall_options.len(),
            underlyings = boot.allocated.hypercall_idx.len(),
            summary_every_s = boot.hypercall_summary_every_s,
            stale_after_ms = stale,
            "hypercall: starting ingress + poller threads"
        );
        let spec = cli::HypercallSpec {
            ws_host: cfg.hypercall_ws_host.clone(),
            rest_host: cfg.hypercall_rest_host.clone(),
            symbols,
            underlyings,
            summary_underlyings: boot.hypercall_options.underlyings.clone(),
            summary_every_s: boot.hypercall_summary_every_s,
            stale_after_ms: stale,
        };
        match cli::spawn_hypercall(
            spec,
            tls_config.clone(),
            hypercall_prod,
            hypercall_event_prod,
            hypercall_opt_prod,
            statuses.hypercall.clone(),
            statuses.hc.clone(),
            11,
            &run_dir,
            epoch_ns,
            raw_tap_cfg.hypercall,
            capture_metrics_for(obs.counter_ids.as_ref().map(|c| c.capture_hypercall)),
        ) {
            Ok(hs) => handles.extend(hs),
            Err(e) => {
                error!(error = ?e, "hypercall: capture open failed");
                join_reverse(handles);
                return ExitCode::from(1);
            }
        }
    } else {
        // The unspawned-venue shape (§3.3): permanently-empty rings, so a
        // boot without `[hypercall]` is the pre-HC5 boot, bit for bit.
        drop(hypercall_prod);
        drop(hypercall_event_prod);
        drop(hypercall_opt_prod);
    }

    if let Some(polygon_path) = args.polygon_path {
        match WssEndpoint::resolve(&cfg.alchemy_host, 443, &polygon_path) {
            Ok(rpc_ep) => {
                match spawn_rpc(
                    rpc_ep,
                    tls_config.clone(),
                    rpc_prod,
                    statuses.rpc.clone(),
                    3,
                    &run_dir,
                    epoch_ns,
                    raw_tap_cfg.rpc,
                    capture_metrics_for(obs.counter_ids.as_ref().map(|c| c.capture_rpc)),
                ) {
                    Ok(h) => handles.push(h),
                    Err(e) => {
                        error!(error = ?e, "rpc: capture open failed");
                        join_reverse(handles);
                        return ExitCode::from(1);
                    }
                }
            }
            Err(e) => {
                error!(error = ?e, "RPC DNS failed; skipping rpc ingress");
            }
        }
    } else {
        warn!("--polygon-path not provided; RPC ingress thread not started");
    }

    // -- HYPARB H3b: the HyperEVM pool-event ingress --
    // Both switches or nothing: the path flag AND a `[hyperevm] pools`
    // list. Anything else drops the producer, so the engine's pool lane
    // is a permanently-empty ring (the unspawned-venue shape, §3.3).
    // HYPARB H5: whether the operator CONFIGURED the ingress — slot 0
    // refuses a boot without it (a member that can never see a pool);
    // a runtime failure (DNS, a dishonest archive) only darkens the
    // member (O-H15), it never refuses the boot.
    let hyperevm_configured = args.hyperevm_path.is_some() && !boot.allocated.hyperevm.is_empty();
    match (
        args.hyperevm_path.as_deref(),
        boot.allocated.hyperevm.is_empty(),
    ) {
        (Some(path), false) => {
            let table = match cli::hyperevm_pool_table(&boot.allocated) {
                Ok(t) => t,
                Err(e) => {
                    error!(error = ?e, "hyperevm: pool table refused");
                    join_reverse(handles);
                    return ExitCode::from(1);
                }
            };
            match WssEndpoint::resolve(&cfg.hyperevm_ws_host, 443, path) {
                Ok(ep) => {
                    info!(
                        host = %cfg.hyperevm_ws_host,
                        pools = boot.allocated.hyperevm.len(),
                        "hyperevm: starting pool-event ingress"
                    );
                    match cli::spawn_hyperevm(
                        ep,
                        tls_config.clone(),
                        hyperevm_prod,
                        statuses.hyperevm.clone(),
                        10,
                        &run_dir,
                        epoch_ns,
                        raw_tap_cfg.hyperevm,
                        capture_metrics_for(obs.counter_ids.as_ref().map(|c| c.capture_hyperevm)),
                        table,
                        ingress_hyperevm::run_loop::DEFAULT_SNAPSHOT_RADIUS,
                    ) {
                        Ok(h) => handles.push(h),
                        Err(e) => {
                            error!(error = ?e, "hyperevm: capture open failed");
                            join_reverse(handles);
                            return ExitCode::from(1);
                        }
                    }
                }
                Err(e) => {
                    error!(error = ?e, "HyperEVM DNS failed; skipping the pool-event ingress");
                }
            }
        }
        (Some(_), true) => {
            warn!("--hyperevm-path given but [hyperevm] pools is empty; pool ingress not started");
            drop(hyperevm_prod);
        }
        (None, _) => drop(hyperevm_prod),
    }

    // -- AI-command ingress (Phase 8f; opt-in via AI_INGRESS_HMAC_KEY
    // in .env) --
    // Key semantics: ABSENT/empty ⇒ thread not started (back-compat
    // with pre-8f .env files); PRESENT but unparseable ⇒ fatal boot
    // error — a typo'd key must never silently disable the AI lane.
    // The parsed key is moved into the thread and never logged.
    match std::env::var("AI_INGRESS_HMAC_KEY") {
        Ok(hex) if !hex.trim().is_empty() => match cli::parse_ai_hmac_key(&hex) {
            Ok(key) => {
                info!(
                    sock = %cfg.ai_ingress_sock,
                    ruleset_dir = %cfg.ai_ruleset_dir,
                    "ingress-ai: starting thread"
                );
                let ai_handle = match cli::spawn_ai(
                    PathBuf::from(&cfg.ai_ingress_sock),
                    PathBuf::from(&cfg.ai_ruleset_dir),
                    key,
                    ai_prod,
                    ruleset_table_prod,
                    ai_universe,
                    ai_descriptors.clone(),
                    ai_status.clone(),
                    4,
                    &run_dir,
                    epoch_ns,
                    capture_metrics_for(obs.counter_ids.as_ref().map(|c| c.capture_ai)),
                ) {
                    Ok(h) => h,
                    Err(e) => {
                        error!(error = ?e, "ingress-ai: capture open failed");
                        join_reverse(handles);
                        return ExitCode::from(1);
                    }
                };
                handles.push(ai_handle);
            }
            Err(reason) => {
                // `reason` is a static description; no key material.
                error!(
                    reason,
                    "AI_INGRESS_HMAC_KEY present but invalid — refusing to boot"
                );
                join_reverse(handles);
                return ExitCode::from(1);
            }
        },
        _ => {
            info!("AI_INGRESS_HMAC_KEY unset; ingress-ai thread not started");
            drop(ai_prod);
            // Same unspawned shape for the table ring: no producer,
            // the parked consumer reads empty forever.
            drop(ruleset_table_prod);
        }
    }

    // -- Engine-thread fills capture (Phase 8f item 6) --
    // engine-fills.pmlr in the per-run capture directory: the
    // positions/P&L feed for the research loop. Open failure is a
    // fatal boot error (§6.5 stance: capture is the product).
    let obs = match cli::open_fills_capture(&run_dir, epoch_ns) {
        Ok(cap) => obs.with_fills_capture(cap),
        Err(e) => {
            error!(error = ?e, dir = %run_dir.display(), "fills capture open failed");
            join_reverse(handles);
            return ExitCode::from(1);
        }
    };
    // M4.1: the order-intent log opens beside the fills capture —
    // same §6.5 stance (capture is the product; open failure fatal).
    let obs = match cli::open_orders_capture(&run_dir, epoch_ns) {
        Ok(cap) => obs.with_orders_capture(cap),
        Err(e) => {
            error!(error = ?e, dir = %run_dir.display(), "orders capture open failed");
            join_reverse(handles);
            return ExitCode::from(1);
        }
    };

    // -- Main thread: real engine loop until SIGINT --
    let cons = Consumers {
        tick_lanes: [
            pm_lane_cons,
            bn_lane_cons,
            okx_lane_cons,
            deribit_lane_cons,
            hl_lane_cons,
            bybit_lane_cons,
            mexc_lane_cons,
            hypercall_lane_cons,
        ],
        event_lanes: event_lane_cons,
        depth_lanes: depth_lane_cons,
        opt_lanes: opt_lane_cons,
        rpc_signal: rpc_cons,
        hyperevm_signal: hyperevm_cons,
        fill_lanes: fill_lane_cons,
        ai_cmds: ai_lane_cons,
        ai_status,
        ruleset_tables: ruleset_table_cons,
    };
    let engine_cfg = EngineConfig {
        // M1: pairs from the resolved universe ([pairs] map or the
        // default first-PM × first-BN-spot).
        pairs: boot
            .allocated
            .pairs
            .iter()
            .map(|&(pm, bn)| StrategyPair {
                polymarket: pm,
                binance: bn,
            })
            .collect(),
        threshold_1e6: args.threshold_1e6,
        qty_1e6: args.qty_1e6,
        cooldown_ns: args.cooldown_ns,
    };

    // Observability (`obs`) + latency-dump destination were already
    // built above, before the ingress spawns (Part B.4 needs the
    // registry at spawn time).

    // Boot the /metrics HTTP server if requested. Owns its own
    // thread; observes the same SHUTDOWN flag.
    let mut obs_handles: Vec<std::thread::JoinHandle<()>> = Vec::new();
    if let Some(reg) = obs.metrics.clone() {
        let bind: std::net::SocketAddr = cfg
            .metrics_bind
            .parse()
            .unwrap_or_else(|_| "127.0.0.1:9191".parse().unwrap());
        let stop_ref: &'static AtomicBool = &SHUTDOWN;
        // RG6: `/state` reads the snapshot cell through a writer that
        // owns its own boxed scratch (built inside the thread).
        let state_cell = obs.state.clone();
        obs_handles.push(
            std::thread::Builder::new()
                .name("metrics-http".into())
                .spawn(move || {
                    info!(%bind, state = state_cell.is_some(), "metrics: HTTP server starting");
                    let state = state_cell.map(state_writer);
                    // Non-fatal serve events land here so they carry
                    // the standard tracing timestamp (G1 remediation
                    // item 2 — the old in-crate eprintln had neither
                    // timestamp nor level). WARN, not ERROR: scrape
                    // clients retry; the soak "no ERROR" grep must not
                    // trip on a benign scrape hiccup.
                    let on_event = |ev: core_metrics::MetricsServeEvent<'_>| match ev {
                        core_metrics::MetricsServeEvent::ConnError(e) => {
                            warn!(error = %e, "metrics: connection error")
                        }
                        core_metrics::MetricsServeEvent::AcceptError(e) => {
                            warn!(error = %e, "metrics: accept error")
                        }
                    };
                    if let Err(e) =
                        core_metrics::serve_metrics(bind, reg, state, stop_ref, on_event)
                    {
                        error!(error = ?e, "metrics: serve_metrics returned error");
                    }
                })
                .expect("spawn metrics thread"),
        );
    }

    // Boot the TUI render thread if requested (RG6: it renders the
    // same `/state` snapshot the metrics server serves).
    if let Some(cell) = obs.state.clone().filter(|_| args.tui) {
        let stop_ref: &'static AtomicBool = &SHUTDOWN;
        obs_handles.push(
            std::thread::Builder::new()
                .name("tui-render".into())
                .spawn(move || {
                    if let Err(e) = tui::run_dashboard(&cell, stop_ref) {
                        error!(error = ?e, "tui: run_dashboard returned error");
                    }
                })
                .expect("spawn tui thread"),
        );
    }

    // `--exec` is honoured ONLY by the composed strategy-set arm — it
    // is the only arm that builds a `RoutedDispatcher`. Accepting the
    // flag for `rule-tree` / `ev` and then routing
    // nothing would be the worst kind of silent no-op: the operator
    // passed an arming artifact and the engine ignored it.
    if args.exec.is_some() && !STRATEGY_SET_NAMES.contains(&args.strategy.as_str()) {
        error!(
            strategy = %args.strategy,
            "exec: --exec is only supported by the composed strategy set \
             (the other arms have no routed dispatcher) — boot aborted"
        );
        join_reverse(handles);
        return ExitCode::from(1);
    }
    let strategy_choice = args.strategy.as_str();
    let result = match (strategy_choice, args.live) {
        // HYPARB H0 (O-H1): `latency-arb` has no arm any more — slot 0
        // is the hyparb set member and the old name falls through to
        // the "unknown --strategy" refusal below, on purpose.
        // XSD-S (2026-09-12): `cross-arb` has no arm any more — slot 2 is
        // vacant until `strategy-xsd` lands (XSD-3) and the name falls
        // through to the "unknown --strategy" refusal below, on purpose.
        ("rule-tree", _live) => {
            let rp = match args.rules_path.as_deref() {
                Some(p) => p,
                None => {
                    error!("--strategy rule-tree requires --rules-path <JSON>");
                    join_reverse(handles);
                    return ExitCode::from(1);
                }
            };
            // v1: map every loaded rule to `--polymarket-sym-id`
            // with the rule name's first 16 bytes as the keyword.
            let mut kw = [0u8; 16];
            let n = b"halving".len().min(16);
            kw[..n].copy_from_slice(&b"halving"[..n]);
            let mapping = vec![(boot.allocated.pm_tokens[0].sym, kw, n as u8)];
            info!("running rule-tree PAPER — no orders will be submitted");
            engine_loop_rule_tree_full(
                cons,
                engine_cfg,
                clob_dispatcher::PaperDispatcher::new(),
                obs,
                rp,
                &mapping,
            )
        }
        ("ev", live_flag) => {
            let path = match args.artifacts_path.as_deref() {
                Some(p) => p,
                None => {
                    error!("--strategy ev requires --artifacts-path <NDJSON>");
                    join_reverse(handles);
                    return ExitCode::from(1);
                }
            };
            if live_flag {
                match boot_queued_live(&cfg, tls_config.clone()) {
                    Ok((queued, worker_handle)) => {
                        info!("running ev LIVE — orders queued to dispatcher thread");
                        obs_handles.push(worker_handle);
                        engine_loop_ev_full(cons, engine_cfg, queued, obs, path)
                    }
                    Err(reason) => EngineLoopResult::Failed(reason),
                }
            } else {
                info!("running ev PAPER — no orders will be submitted");
                engine_loop_ev_full(
                    cons,
                    engine_cfg,
                    clob_dispatcher::PaperDispatcher::new(),
                    obs,
                    path,
                )
            }
        }
        (name, _live) if STRATEGY_SET_NAMES.contains(&name) => {
            // Phase 8f item 7: the composed StrategySet. `all` means
            // "every built member the given flags can boot" —
            // hyparb only when `hyparb.toml` resolves (H5), bin15 only
            // when its artifact resolves, vrp only when
            // `vrp.toml` resolves (VRP V7: slot 1), icdp only when its
            // artifact resolves (slot 2 is vacant — XSD-S),
            // ai-exec and vm unconditionally (neither has boot
            // config; items 8 / 8g-6) (members without config boot
            // inert; see engine_loop_set_full docs). `ai-exec` (item
            // 8b) and `vm` (8g item 6) are single-bit sets per §7
            // "single name = single bit" — no standalone path exists
            // for either; vm boots inert until a ruleset table is
            // staged + committed (8g §7.3 — normal, not an error).
            // PAPER-only until the 8i RiskGate lands — the set has no
            // live arm.
            let requested =
                strategy_set::mask_for_name(name).expect("matched names are valid mask names");
            // ICDP I5: resolve the artifact against the SAME
            // descriptor table the ruleset validator uses (D-6 truth).
            // Only when the bit is requested — `--strategy ai` never
            // touches the file.
            let icdp_params = if requested & strategy_set::BIT_ICDP != 0 {
                match load_icdp_params(args.icdp.as_deref(), &ai_descriptors) {
                    Ok(p) => Some(p),
                    Err(reason) => {
                        error!(reason, "icdp: artifact refused — boot aborted");
                        join_reverse(handles);
                        return ExitCode::from(1);
                    }
                }
            } else {
                None
            };
            // RG2: the regime detector's artifact + seed, resolved
            // against the same descriptor table (D-6 truth). An absent
            // DEFAULT file is legal (unconfigured); an explicit path
            // or a present-but-invalid file refuses the boot.
            let regime_boot = match cli::regime_boot::load_regime_boot(
                args.regime.as_deref(),
                args.regime_seed.as_deref(),
                &ai_descriptors,
            ) {
                Ok(rb) => rb,
                Err(reason) => {
                    error!(reason, "regime: artifact refused — boot aborted");
                    join_reverse(handles);
                    return ExitCode::from(1);
                }
            };
            // VRP V7: the member's artifact, its chain table and its
            // boot seed, all resolved against the same descriptor table
            // (D-6 truth). F19: ONLY when the bit is requested. This
            // ran for every set boot, so `STRATEGY=ai` with a
            // present-but-corrupt `vrp.toml` refused the boot — and the
            // wrapper's documented rollback IS "drop the mask back to
            // `ai`", which could therefore never escape a corrupt VRP
            // file while KeepAlive relaunched into the same refusal. It
            // also configured, seeded and restored a member whose bit
            // was clear, so a runtime `EnableStrategy(1)` would have
            // traded an armed member nobody enabled.
            //
            // An ABSENT seed is still legal — a cold boot must be, the
            // engine restarts about three times a day — and the member
            // simply holds. A file that is PRESENT and unreadable
            // refuses the boot: a seed the engine cannot read exactly is
            // a fit nobody measured.
            // Operator ruling 2026-09-15: an EMPTY VENUE CHAIN drops
            // slot 1 and lets the rest of the engine boot; an artifact
            // problem still refuses. `vrp_dropped_chain_empty` carries
            // that decision to the F19 check below and to `/metrics`.
            let mut vrp_dropped_chain_empty = false;
            let vrp_boot = if cli::vrp_boot::vrp_wanted(requested) {
                match cli::vrp_boot::load_vrp_boot(
                    args.vrp.as_deref(),
                    args.vrp_seed.as_deref(),
                    args.vrp_state.as_deref(),
                    &|d: &str| ai_descriptors.resolve(d.as_bytes()).map(|(sym, _)| sym),
                    &discovery.deribit_options,
                ) {
                    Ok(v) => v,
                    // Operator error. F19 stands: refuse.
                    Err(cli::vrp_boot::VrpBootError::Refused(reason)) => {
                        error!(reason, "vrp: artifact refused — boot aborted");
                        join_reverse(handles);
                        return ExitCode::from(1);
                    }
                    // Venue condition. Drop slot 1, boot the rest.
                    //
                    // This is a DELIBERATE, operator-ruled departure
                    // from F19, and the whole objection to it is that a
                    // silently-dropped member is one nobody notices. So
                    // it is not silent: ERROR level, the reason in
                    // full, what is being given up, and what will bring
                    // it back. `/state` shows it too — `requested_mask`
                    // keeps the vrp bit (it IS what was asked for)
                    // while `enabled_mask` does not, and that
                    // divergence is the machine-readable signal.
                    Err(cli::vrp_boot::VrpBootError::VenueChainEmpty(reason)) => {
                        error!(
                            reason,
                            "vrp: DROPPED from this boot — the venue supplied no options \
                             chain. Slot 1 will NOT trade for the life of this run (the \
                             chain is discovered once, at boot). Every other member boots \
                             normally. The next restart picks vrp up again if the venue \
                             has recovered by then."
                        );
                        vrp_dropped_chain_empty = true;
                        None
                    }
                }
            } else {
                None
            };
            // F19: requested-but-absent REFUSES (the icdp law). The old
            // shape booted `ai+vrp` silently as `ai` when the default
            // `vrp.toml` was missing — `configured` simply lacked the
            // bit, the composed mask was still non-zero, and nothing
            // said the strategy the operator asked for was not there.
            // `vrp_dropped_chain_empty` is the ONE exemption: the
            // artifact was there and was good, so "the artifact is
            // absent" would be a false statement, and the operator has
            // ruled that a venue outage must not take the engine down.
            if cli::vrp_boot::vrp_wanted(requested)
                && vrp_boot.is_none()
                && !vrp_dropped_chain_empty
            {
                error!(
                    "vrp: requested by --strategy but the artifact is absent \
                     (~/multivenue/vrp.toml or --vrp) — boot aborted"
                );
                join_reverse(handles);
                return ExitCode::from(1);
            }
            // XSD-3: the member's four artifacts, resolved against the
            // same descriptor table; only when the bit is requested
            // (`--strategy ai` never touches the files). An ABSENT
            // default `xsd.toml` or table leaves the member unconfigured
            // and its bit unset (the vrp law); a present-and-wrong file
            // refuses the boot.
            let xsd_boot = if requested & strategy_set::BIT_XSD != 0 {
                match cli::xsd_boot::load_xsd_boot(
                    args.xsd.as_deref(),
                    args.xsd_table.as_deref(),
                    args.xsd_seed.as_deref(),
                    args.xsd_state.as_deref(),
                    &|d: &str| ai_descriptors.resolve(d.as_bytes()).map(|(sym, _)| sym),
                    cli::xsd_boot::wall_hour_now(),
                ) {
                    Ok(b) => b,
                    Err(reason) => {
                        error!(reason, "xsd: artifact refused — boot aborted");
                        join_reverse(handles);
                        return ExitCode::from(1);
                    }
                }
            } else {
                None
            };
            // BIN15 O4b: slot 3's artifact, resolved against the same
            // descriptor table. The Yes slots come from the UNIVERSE
            // (the rolling ordinals it allocated), not from a
            // descriptor: a rolling slot's venue coin is rebound per
            // instance, so it has no stable descriptor to resolve.
            let rolling_syms: Vec<core_types::SymbolId> = (0..boot.hl_rolling.len())
                .map(|f| ingress_hyperliquid::family::rolling_sym(f, 0))
                .collect();
            let bin15_boot = if cli::bin15_boot::bin15_wanted(requested) {
                match cli::bin15_boot::load_bin15_boot(
                    args.bin15.as_deref(),
                    args.bin15_seed_dir.as_deref(),
                    &|d: &str| ai_descriptors.resolve(d.as_bytes()).map(|(sym, _)| sym),
                    &boot.hl_rolling,
                    &rolling_syms,
                ) {
                    Ok(b) => b,
                    Err(reason) => {
                        error!(reason, "bin15: artifact refused — boot aborted");
                        join_reverse(handles);
                        return ExitCode::from(1);
                    }
                }
            } else {
                None
            };
            // F19 / the icdp law: requested-but-absent REFUSES. Booting
            // `ai+bin15` silently as `ai` is how an operator comes to
            // watch a member that was never there.
            if cli::bin15_boot::bin15_wanted(requested) && bin15_boot.is_none() {
                error!(
                    "bin15: requested by --strategy but the artifact is absent \
                     (~/multivenue/bin15.toml or --bin15) — boot aborted"
                );
                join_reverse(handles);
                return ExitCode::from(1);
            }
            // HYPARB H5: slot 0's artifact. Coins resolve against the
            // same descriptor table; pools against the universe's
            // `[hyperevm]` list (the universe allocates their symbols).
            let hyparb_boot = if cli::hyparb_boot::hyparb_wanted(requested) {
                match cli::hyparb_boot::load_hyparb_boot(
                    args.hyparb.as_deref(),
                    &|d: &str| ai_descriptors.resolve(d.as_bytes()).map(|(sym, _)| sym),
                    &boot.allocated.hyperevm,
                    args.evm_testnet,
                ) {
                    Ok(b) => b,
                    Err(reason) => {
                        error!(reason, "hyparb: artifact refused — boot aborted");
                        join_reverse(handles);
                        return ExitCode::from(1);
                    }
                }
            } else {
                if args.evm_testnet {
                    error!("--evm-testnet without slot 0 in --strategy — boot aborted");
                    join_reverse(handles);
                    return ExitCode::from(1);
                }
                None
            };
            // F19 / the icdp law: requested-but-absent REFUSES.
            if cli::hyparb_boot::hyparb_wanted(requested) && hyparb_boot.is_none() {
                error!(
                    "hyparb: requested by --strategy but the artifact is absent \
                     (~/multivenue/hyparb.toml or --hyparb) — boot aborted"
                );
                join_reverse(handles);
                return ExitCode::from(1);
            }
            if hyparb_boot.is_some() && !hyperevm_configured {
                error!(
                    "hyparb: the member needs the HyperEVM pool ingress — \
                     --hyperevm-path and a non-empty universe.toml `[hyperevm] pools` \
                     — boot aborted"
                );
                join_reverse(handles);
                return ExitCode::from(1);
            }
            // HYPARB L5 (O-HL1): live mode's three switches — the
            // artifact's `mode = "live"`, and the exec interlock's slot 0
            // (`exec.toml [exec.slot.0] mode = "live"` + `--arm-live 0`,
            // which `exec_boot::resolve` already made agree) — agree, or
            // no boot. Either side alone is a refusal, never a downgrade.
            let hyparb_live_mode = hyparb_boot
                .as_ref()
                .is_some_and(|b| b.mode == core_config::hyparb::HyparbMode::Live);
            let slot0_armed = exec_boot
                .as_ref()
                .is_some_and(cli::exec_boot::ExecBoot::hyparb_live);
            if hyparb_live_mode != slot0_armed {
                error!(
                    artifact_live = hyparb_live_mode,
                    exec_slot0_live = slot0_armed,
                    "hyparb: LIVE mode needs all three switches — hyparb.toml `mode = \"live\"`, \
                     exec.toml [exec.slot.0] `mode = \"live\"` and --arm-live 0 — and they \
                     disagree; boot aborted"
                );
                join_reverse(handles);
                return ExitCode::from(1);
            }
            // RG6: the `/state` `boot` section's regime identity.
            let mut obs = obs;
            if let Some(rb) = regime_boot.as_ref() {
                obs.boot.regime_hash = rb.hash;
                obs.boot.regime_configured = 1;
            }
            // HYPARB H8: the EVM write path, TESTNET ONLY (O-H5) — the
            // shadow of each paper AMM decision (O-H12). The chain checks
            // need the wire, so they run here. H9 R3: a misconfiguration
            // or a VERIFIED interlock failure refuses the boot; an
            // endpoint that cannot be reached (transport, rate limit) or
            // an unfunded wallet 0 leaves the shadow DARK — nothing is
            // sent, so an interlock that could not be verified is still
            // not passed, and the paper member (the P&L source) runs.
            match hyparb_boot
                .as_ref()
                .filter(|b| b.mode == core_config::hyparb::HyparbMode::Testnet)
            {
                Some(hb) => {
                    let Some(t) = hb.testnet.as_ref() else {
                        error!("hyparb: testnet mode without [testnet] — boot aborted");
                        join_reverse(handles);
                        return ExitCode::from(1);
                    };
                    let read_url = format!(
                        "https://{}{}",
                        cfg.hyperevm_ws_host,
                        args.hyperevm_path.as_deref().unwrap_or("/")
                    );
                    let booted = cli::evm_testnet::wallet_keys_from_env(t.wallets)
                        .map_err(cli::evm_testnet::ShadowBootErr::Refuse)
                        .and_then(|k| {
                            cli::evm_testnet::boot_shadow(
                                t,
                                &k.keys,
                                k.source,
                                hb.params.gas_p99_usd_1e6,
                                &read_url,
                                args.evm_hybrid,
                                tls_config.clone(),
                            )
                        });
                    match booted {
                        Ok(b) => {
                            warn!("{}", b.tell);
                            obs.hyparb_shadow = Some(b.tap);
                            handles.push(b.handle);
                        }
                        Err(cli::evm_testnet::ShadowBootErr::Dark(reason)) => {
                            obs.hyparb_shadow_dark = true;
                            error!(
                                reason,
                                "hyparb: the EVM write path is DARK — no testnet shadow swap \
                                 will be sent this run; the paper member runs (fix the reason \
                                 and restart to arm it)"
                            );
                        }
                        Err(cli::evm_testnet::ShadowBootErr::Refuse(reason)) => {
                            error!(reason, "hyparb: the EVM write path refused — boot aborted");
                            join_reverse(handles);
                            return ExitCode::from(1);
                        }
                    }
                }
                None if args.evm_hybrid => {
                    error!("--evm-hybrid without a testnet hyparb artifact — boot aborted");
                    join_reverse(handles);
                    return ExitCode::from(1);
                }
                None => {}
            }
            match exec_boot {
                // NO `--exec`: the pre-E1 path, untouched. Same
                // dispatcher type, same call, same everything.
                None => {
                    info!("running strategy-set PAPER — no orders will be submitted");
                    engine_loop_set_full(
                        cons,
                        clob_dispatcher::PaperDispatcher::new(),
                        obs,
                        requested,
                        vrp_boot.as_ref(),
                        xsd_boot.as_ref(),
                        bin15_boot.as_ref(),
                        icdp_params.as_ref(),
                        regime_boot.as_ref(),
                        hyparb_boot.as_ref(),
                    )
                }
                // WITH `--exec`: the same loop over the compositing
                // dispatcher. `engine_loop_set_full` is generic over
                // `D: OrderDispatch`, so the loop itself is unchanged —
                // `RoutedDispatcher` is just another concrete `D`.
                //
                // The live arm is `NullLiveDispatcher` in E1: it refuses
                // every order. That is unreachable on a booted engine
                // (a live slot with no compiled arm was already refused
                // above), and it is what makes LAW E-1 — a live slot
                // never falls back to paper — true by construction
                // rather than by care.
                Some(eb) => {
                    cli::exec_boot::log_boot_tell(&eb);
                    // **HYPARB L5 — slot 0's own live arm** (O-HL1/O-HL3):
                    // real swaps on HyperEVM mainnet and real hedges from
                    // the slot's own Hyperliquid account, authorised only
                    // by all three switches (checked above, and again by
                    // `MainnetAuthority::armed_engine`).
                    let hyparb_live = if eb.hyparb_live() {
                        match boot_hyparb_live_arm(&eb, hyparb_boot.as_ref(), &boot.allocated) {
                            Ok(b) => {
                                warn!("{}", b.tell);
                                obs.hyparb_live_status = Some(b.arm.shared().clone());
                                handles.push(b.handle);
                                Some(b.arm)
                            }
                            Err(reason) => {
                                error!(reason, "hyparb: the LIVE arm refused — boot aborted");
                                join_reverse(handles);
                                return ExitCode::from(1);
                            }
                        }
                    } else {
                        None
                    };
                    // **E7 — the operator's Hyperliquid arm**, for every
                    // live slot on Hyperliquid other than slot 0, fed by
                    // fill lane 3's producer and configured from the
                    // `HYPERLIQUID_*` variables the wrapper sourced.
                    let hl_arm = if eb.hl_arm_needed() {
                        let Some(f3p) = hl_fill_prod.take() else {
                            error!("exec: fill lane 3 producer already taken — boot aborted");
                            join_reverse(handles);
                            return ExitCode::from(1);
                        };
                        match boot_operator_hl_arm(&eb, &cfg.hyperliquid_ws_host, f3p) {
                            Ok(a) => Some(a),
                            Err(reason) => {
                                error!(reason, "exec: hyperliquid arm refused — boot aborted");
                                join_reverse(handles);
                                return ExitCode::from(1);
                            }
                        }
                    } else {
                        None
                    };
                    // The dispatcher types cannot share one binding (the
                    // loop is monomorphised over `D`), so each shape enters
                    // the loop from its own arm; the halt wiring is one
                    // generic helper. Nothing armed keeps the refusing
                    // stub, so LAW E-1 stays true by construction.
                    let anchor = core_time::WallAnchor::now();
                    let paper = clob_dispatcher::PaperDispatcher::new();
                    let live_line = cli::exec_boot::render_slot_mask(eb.live_mask);
                    match (hl_arm, hyparb_live) {
                        (Some(arm), None) => {
                            let mut d =
                                exec_router::RoutedDispatcher::new(eb.route, paper, arm, anchor);
                            wire_exec_halts(&mut d, &eb, halt_mask);
                            info!(
                                live = %live_line,
                                "running strategy-set with LIVE slots — real orders will be \
                                 submitted"
                            );
                            engine_loop_set_full(
                                cons,
                                d,
                                obs,
                                requested,
                                vrp_boot.as_ref(),
                                xsd_boot.as_ref(),
                                bin15_boot.as_ref(),
                                icdp_params.as_ref(),
                                regime_boot.as_ref(),
                                hyparb_boot.as_ref(),
                            )
                        }
                        (Some(arm), Some(live)) => {
                            let split =
                                exec_router::SlotSplit::new(cli::hyparb_live::SLOT, arm, live);
                            let mut d =
                                exec_router::RoutedDispatcher::new(eb.route, paper, split, anchor);
                            wire_exec_halts(&mut d, &eb, halt_mask);
                            info!(
                                live = %live_line,
                                "running strategy-set with LIVE slots on TWO arms (slot 0 on its \
                                 own wallet) — real orders will be submitted"
                            );
                            engine_loop_set_full(
                                cons,
                                d,
                                obs,
                                requested,
                                vrp_boot.as_ref(),
                                xsd_boot.as_ref(),
                                bin15_boot.as_ref(),
                                icdp_params.as_ref(),
                                regime_boot.as_ref(),
                                hyparb_boot.as_ref(),
                            )
                        }
                        (None, Some(live)) => {
                            let split = exec_router::SlotSplit::new(
                                cli::hyparb_live::SLOT,
                                exec_router::NullLiveDispatcher::new(),
                                live,
                            );
                            let mut d =
                                exec_router::RoutedDispatcher::new(eb.route, paper, split, anchor);
                            wire_exec_halts(&mut d, &eb, halt_mask);
                            info!(
                                live = %live_line,
                                "running strategy-set with slot 0 LIVE on its own wallet — real \
                                 orders will be submitted"
                            );
                            engine_loop_set_full(
                                cons,
                                d,
                                obs,
                                requested,
                                vrp_boot.as_ref(),
                                xsd_boot.as_ref(),
                                bin15_boot.as_ref(),
                                icdp_params.as_ref(),
                                regime_boot.as_ref(),
                                hyparb_boot.as_ref(),
                            )
                        }
                        (None, None) => {
                            // Nothing armed: the refusing stub, so a live
                            // slot on a venue with no arm cannot exist here
                            // (already refused by `exec_boot::resolve`).
                            //
                            // E6: `WallAnchor::now()` above is the anchor
                            // the venue-fill ledger's 00:00Z day epoch
                            // needs — an `Order`'s `ts_ns` is monotonic and
                            // the day cap is a wall-clock fact. The ledger
                            // is left UNSEEDED: every live PLACE is refused
                            // until the arm's reconciler reports success.
                            let mut d = exec_router::RoutedDispatcher::new(
                                eb.route,
                                paper,
                                exec_router::NullLiveDispatcher::new(),
                                anchor,
                            );
                            wire_exec_halts(&mut d, &eb, halt_mask);
                            info!(
                                "running strategy-set PAPER (exec artifact present, nothing \
                                 armed) — no orders will be submitted"
                            );
                            engine_loop_set_full(
                                cons,
                                d,
                                obs,
                                requested,
                                vrp_boot.as_ref(),
                                xsd_boot.as_ref(),
                                bin15_boot.as_ref(),
                                icdp_params.as_ref(),
                                regime_boot.as_ref(),
                                hyparb_boot.as_ref(),
                            )
                        }
                    }
                }
            }
        }
        (other, _) => {
            error!(strategy = other, "unknown --strategy value");
            join_reverse(handles);
            return ExitCode::from(1);
        }
    };
    let exit_code = match result {
        EngineLoopResult::Done(stats) => {
            info!(?stats, "engine loop exited cleanly");
            ExitCode::SUCCESS
        }
        EngineLoopResult::Failed(reason) => {
            error!(reason, "engine loop failed at boot");
            ExitCode::from(1)
        }
    };

    // -- Reverse-order join (ingress threads first, then obs) --
    join_reverse(handles);
    for h in obs_handles.into_iter().rev() {
        let name = h.thread().name().unwrap_or("<obs>").to_string();
        if let Err(e) = h.join() {
            tracing::error!(thread = %name, error = ?e, "obs thread join panicked");
        }
    }
    info!("clean shutdown");
    exit_code
}

/// ICDP I5: read `icdp.toml`, resolve every descriptor against the boot
/// universe, build the POD artifact the strategy consumes, hash the
/// exact bytes (logged + stamped by the strategy). Boot-only.
fn load_icdp_params(
    path: Option<&std::path::Path>,
    descriptors: &ingress_ai::DescriptorTable,
) -> Result<strategy_icdp::IcdpParams, String> {
    let owned;
    let path: &std::path::Path = match path {
        Some(p) => p,
        None => {
            owned = core_config::icdp::default_icdp_path().map_err(|e| e.to_string())?;
            std::path::Path::new(&owned)
        }
    };
    let (file, bytes) = core_config::icdp::load(path).map_err(|e| e.to_string())?;
    let mut params = strategy_icdp::IcdpParams::EMPTY;
    params.tf_ns = file.tf_ms.saturating_mul(1_000_000);
    params.delta_ns = file.delta_ms.saturating_mul(1_000_000);
    params.n = file.instruments.len();
    params.hash = core_crypto::sha256(&bytes);
    for (i, inst) in file.instruments.iter().enumerate() {
        let (sym, _caps) = descriptors
            .resolve(inst.descriptor.as_bytes())
            .ok_or_else(|| format!("icdp: `{}` is not in the boot universe", inst.descriptor))?;
        params.syms[i] = strategy_icdp::IcdpSymParams {
            sym,
            mu: inst.mu,
            inv_sd: inst.inv_sd,
            w: inst.w,
            b: inst.b,
            thr: inst.thr,
            notional_1e6: inst.notional_usd_1e6,
            spread_cap_1e9: inst.spread_cap_1e9,
            entry_slip_1e9: inst.entry_slip_1e9,
            exit_slip_1e9: inst.exit_slip_1e9,
        };
        info!(
            descriptor = %inst.descriptor,
            sym,
            notional_usd_1e6 = inst.notional_usd_1e6,
            thr_1e9 = inst.thr,
            "icdp: instrument resolved"
        );
    }
    Ok(params)
}
#[cfg(test)]
mod strategy_name_pin {
    //! BIN15 O5: the regression pin for the arm/mask drift recorded on
    //! `STRATEGY_SET_NAMES`. A name that the mask table accepts but this
    //! binary will not boot is a silent dark-engine bug — the wrapper
    //! passes the name, the process starts, capture runs, and only the
    //! mask gauge says the strategies never composed.

    /// Every bootable name must be a name the mask table can resolve —
    /// the arm body `expect`s exactly this.
    #[test]
    fn every_bootable_name_resolves_to_a_mask() {
        let mut i = 0;
        while i < super::STRATEGY_SET_NAMES.len() {
            let name = super::STRATEGY_SET_NAMES[i];
            assert!(
                strategy_set::mask_for_name(name).is_some(),
                "{name} is bootable but mask_for_name does not know it"
            );
            i += 1;
        }
    }

    /// Every name the mask table accepts must be bootable. This is the
    /// direction that failed on 2026-09-12. (HYPARB H0 retired the one
    /// exemption, `latency-arb`, with its standalone arm — O-H1.)
    #[test]
    fn every_mask_name_is_bootable() {
        let mut i = 0;
        while i < strategy_set::MASK_TABLE.len() {
            let (name, _mask) = strategy_set::MASK_TABLE[i];
            assert!(
                super::STRATEGY_SET_NAMES.contains(&name),
                "mask_for_name accepts {name} but the boot arm refuses it"
            );
            i += 1;
        }
    }

    /// HYPARB H0 (O-H1): `latency-arb` is gone as a name — neither the
    /// mask table nor the boot arm knows it, so the old wrapper line
    /// refuses the boot instead of composing a different member — and
    /// the three slot-0 names resolve to bit 0.
    #[test]
    fn latency_arb_is_refused_and_the_hyparb_names_resolve() {
        assert_eq!(strategy_set::mask_for_name("latency-arb"), None);
        assert!(!super::STRATEGY_SET_NAMES.contains(&"latency-arb"));
        let want = [
            ("hyparb", strategy_set::BIT_HYPARB),
            (
                "ai+hyparb",
                strategy_set::BIT_AI_EXEC | strategy_set::BIT_VM | strategy_set::BIT_HYPARB,
            ),
            (
                "ai+vrp+xsd+bin15+hyparb",
                strategy_set::BIT_AI_EXEC
                    | strategy_set::BIT_VM
                    | strategy_set::BIT_VRP
                    | strategy_set::BIT_XSD
                    | strategy_set::BIT_BIN15
                    | strategy_set::BIT_HYPARB,
            ),
        ];
        let mut i = 0;
        while i < want.len() {
            let (name, mask) = want[i];
            assert!(super::STRATEGY_SET_NAMES.contains(&name), "{name}");
            assert_eq!(strategy_set::mask_for_name(name), Some(mask), "{name}");
            i += 1;
        }
    }

    /// The bin15 names specifically, spelled out, so the 2026-09-12
    /// omission cannot come back unnoticed even if both lists are edited.
    #[test]
    fn the_five_bin15_names_boot() {
        let want = [
            ("bin15", strategy_set::BIT_BIN15),
            (
                "ai+bin15",
                strategy_set::BIT_AI_EXEC | strategy_set::BIT_VM | strategy_set::BIT_BIN15,
            ),
            (
                "ai+vrp+bin15",
                strategy_set::BIT_AI_EXEC
                    | strategy_set::BIT_VM
                    | strategy_set::BIT_VRP
                    | strategy_set::BIT_BIN15,
            ),
            (
                "ai+xsd+bin15",
                strategy_set::BIT_AI_EXEC
                    | strategy_set::BIT_VM
                    | strategy_set::BIT_XSD
                    | strategy_set::BIT_BIN15,
            ),
            (
                "ai+vrp+xsd+bin15",
                strategy_set::BIT_AI_EXEC
                    | strategy_set::BIT_VM
                    | strategy_set::BIT_VRP
                    | strategy_set::BIT_XSD
                    | strategy_set::BIT_BIN15,
            ),
        ];
        let mut i = 0;
        while i < want.len() {
            let (name, mask) = want[i];
            assert!(
                super::STRATEGY_SET_NAMES.contains(&name),
                "{name} is not in STRATEGY_SET_NAMES"
            );
            assert_eq!(strategy_set::mask_for_name(name), Some(mask), "{name}");
            i += 1;
        }
    }
}
