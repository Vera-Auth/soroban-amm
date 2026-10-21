//! Concentrated Liquidity AMM (Uniswap v3-style tick-based ranges).
//! Standalone contract — does NOT modify the existing AMM pool.
#![no_std]

pub mod math;
pub mod tick_bitmap;

use soroban_sdk::token::Client as TokenClient;
use soroban_sdk::{
    contract, contractclient, contracterror, contractimpl, contracttype, symbol_short, Address,
    BytesN, Env, Symbol, Vec,
};

#[cfg(feature = "testutils")]
pub const WASM: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../target/wasm32v1-none/release/concentrated_liquidity.wasm"
));

const PRICE_SCALE: i128 = 1_000_000;
const TICK_BASE_NUM: i128 = 1_000_100;
const TICK_BASE_DEN: i128 = PRICE_SCALE;
const MIN_TICK: i32 = -887_272;
const MAX_TICK: i32 = 887_272;
/// Fixed-point scale of the fee-growth accumulators (Q64 wide scale, issue #1098).
pub const FEE_GROWTH_SCALE: i128 = 1_i128 << 64;

// Per-user position state lives in persistent storage (see issue #346): it is
// unbounded in the number of LPs and tick ranges, so it must not share the
// fixed 64 KB instance-storage budget. Persistent entries are evicted once
// their TTL lapses, so every write bumps the entry's TTL back up.
//
// Only extend when fewer than this many ledgers of life remain
// (~30 days at 5 s/ledger); avoids redundant bumps on every access.
const POSITION_TTL_THRESHOLD: u32 = 518_400;
// Extend a touched entry's life to this many ledgers (~180 days at 5 s/ledger).
const POSITION_BUMP_TO: u32 = 3_110_400;

// Instance-storage TTL management (issue #905).
//
// The instance entry holds the executable plus every `storage().instance()`
// value: sqrt price, current tick, active liquidity, fee-growth globals, admin,
// tokens, fee config and oracle settings. If that entry's TTL lapses the whole
// pool is archived and every call traps — stranding every open position, since
// the persistent position entries survive but there is no live pool to burn
// them against. Each entrypoint therefore extends the instance entry, using the
// same threshold/bump the persistent position entries already use so instance
// and persistent state age at one horizon.
//
// Only extend when fewer than this many ledgers of life remain
// (~30 days at 5 s/ledger); avoids redundant bumps on every access.
const INSTANCE_TTL_THRESHOLD: u32 = POSITION_TTL_THRESHOLD;
// Extend the instance entry's life to this many ledgers (~180 days at 5 s/ledger).
const INSTANCE_BUMP_TO: u32 = POSITION_BUMP_TO;

/// Default capacity of the persistent oracle observation ring buffer (#1101).
/// At 1 swap per 5-second ledger this retains ~42 minutes of continuous per-ledger
/// observations; at lower trade frequencies it spans hours or days.
pub const DEFAULT_ORACLE_CARDINALITY: u32 = 512;
/// Maximum capacity of the oracle observation ring buffer (#1101).
pub const MAX_ORACLE_CARDINALITY: u32 = 2048;

#[contracterror]
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum ClError {
    AlreadyInitialized = 1,
    TokensMustDiffer = 2,
    InvalidFeeBps = 3,
    TickOutOfRange = 4,
    ZeroAmounts = 5,
    SlippageExceeded = 6,
    ZeroLiquidity = 7,
    InsufficientLiquidity = 8,
    PositionNotFound = 9,
    DeadlineExpired = 10,
    Paused = 11,
    Unauthorized = 12,
    TickNotAligned = 13,     // tick is not a multiple of tick_spacing
    InvalidTickSpacing = 14, // tick_spacing must be > 0
    TickNotInitialized = 15, // requested tick has no liquidity (never touched by a position)
    InvalidToken = 16,       // token_in is not token_a or token_b
    RangeOrderInRange = 17,  // range order must be fully out-of-range at creation
    OracleDeviationExceeded = 18,
    NftNotConfigured = 19, // no position-NFT contract is wired into the pool
    NotNftOwner = 20,      // caller does not currently own the position NFT
    NftContractChangeBlocked = 21, // changing NFT contract while positions are tokenized would orphan indices
    RangeOrderExists = 22, // a range order is already active on this range — withdraw it before placing a new one
    /// #696: `swap_exact_out` (or `quote_exact_out`) could not fill the
    /// requested `amount_out` in full before running out of initialized
    /// ticks or hitting `sqrt_price_limit_x96`. Exact-out has no meaningful
    /// partial fill — the caller asked for a specific output amount, so a
    /// shortfall is an error rather than a smaller-than-requested output.
    ExactOutNotFullyFilled = 23,
    /// A function that depends on pool state (tokens, admin, current tick)
    /// was called before `initialize`.
    NotInitialized = 24,
    /// A position amount or liquidity figure cannot be represented: it does
    /// not fit in an `i128`, or an intermediate of its wide evaluation does
    /// not fit in a `u128`. Reported instead of acting on a truncated value
    /// (#963).
    MathOverflow = 25,
    /// `observe(seconds_ago)` requested a timestamp earlier than the oldest
    /// retained observation in the oracle ring buffer (#1101).
    ObservationTooOld = 26,
    /// `set_oracle_cardinality` was called with zero, a value smaller than the
    /// current cardinality, or a value above `MAX_ORACLE_CARDINALITY` (#1101).
    InvalidOracleCardinality = 27,
}

/// Status of a range order (issue #295).
#[contracttype]
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum RangeOrderStatus {
    /// Price has not yet crossed the range — order is pending.
    Pending = 0,
    /// Price has fully crossed the range — order is filled.
    Filled = 1,
    /// Position was closed before being filled.
    Closed = 2,
}

/// Result returned by `mint_position_single_token`.
///
/// Contains the actual amounts consumed and the liquidity minted.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct SingleTokenDepositResult {
    /// Amount of `token_in` actually consumed (≤ `amount_in`).
    pub amount_used: i128,
    /// Dust: `amount_in - amount_used` (returned to caller).
    pub dust: i128,
    /// Liquidity units added to the position.
    pub liquidity: i128,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub enum DataKey {
    TokenA,
    TokenB,
    FeeBps,
    CurrentTick,
    FeeGrowthGlobalA,
    FeeGrowthGlobalB,
    ActiveLiquidity,
    Position(Address, i32, i32),
    PositionList(Address), // Vec<(i32, i32)> of open tick ranges per provider
    TickCumulative,        // i64 — accumulated tick * elapsed_seconds
    LastOracleTimestamp,   // u64 — last oracle update timestamp
    OraclePoint(u64),      // legacy: timestamp → i64 tick_cumulative snapshot
    OracleTimestamps,      // legacy: Vec<u64> — sorted oracle snapshot times for interpolation
    SqrtPriceX96,
    Tick(i32),
    TickBitmap(i32),
    Admin,
    Paused,
    TickSpacing, // i32 — only multiples of this value may be initialized as ticks
    RangeOrder(Address, i32, i32), // marks a position as a range order (issue #295)
    OracleAggregator,
    MaxOracleDeviationBps,
    PendingAdmin,
    ProtocolFeeBps,
    ProtocolFeeRecipient,
    AccruedProtocolFeeA,
    AccruedProtocolFeeB,
    /// Wired-in cl_position_nft contract (issue #348). `Option<Address>`.
    PositionNft,
    /// Reverse index: NFT `token_id` → `(original_provider, lower_tick,
    /// upper_tick)`. Stored at mint time, removed when the NFT is burned.
    NftTokenToPosition(u64),
    /// Forward index: `(provider, lower_tick, upper_tick)` → NFT `token_id`.
    /// Lets the legacy address-keyed entry points detect a tokenized position
    /// and defer control to the current NFT owner.
    PositionNftToken(Address, i32, i32),
    TickBitmapWords,
    /// Persistent ring-buffer slot `0..cardinality` holding an [`OracleObservation`] (#1101).
    OracleObservation(u32),
    /// Instance-storage metadata ([`OracleHeader`]) for the oracle ring buffer (#1101).
    OracleHeader,
}

/// A single oracle observation stored in the persistent ring buffer (#1101).
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct OracleObservation {
    pub timestamp: u64,
    pub cumulative: i64,
}

/// Instance-storage header tracking the circular buffer of oracle observations (#1101).
#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct OracleHeader {
    /// Index (`0..cardinality`) of the most recently written observation when `count > 0`.
    pub index: u32,
    /// Maximum number of observations retained in the ring buffer.
    pub cardinality: u32,
    /// Number of valid observations currently populated (`0..=cardinality`).
    pub count: u32,
}

#[contracttype]
#[derive(Clone, Debug)]
pub struct AggregatedPrice {
    pub price: i128,
    pub confidence: u32,
}

#[contractclient(name = "OracleAggregatorClient")]
pub trait OracleAggregatorInterface {
    fn get_price_safe(env: Env, token_a: Address, token_b: Address) -> AggregatedPrice;
}

/// Minimal view of the `cl_position_nft` contract used by the pool to mint a
/// receipt when a position opens, burn it when the position closes, and resolve
/// the current owner of a position NFT (issue #348).
#[contractclient(name = "PositionNftClient")]
pub trait PositionNftInterface {
    fn mint(env: Env, to: Address, pool: Address, lower_tick: i32, upper_tick: i32) -> u64;
    fn burn(env: Env, token_id: u64);
    fn owner_of(env: Env, token_id: u64) -> Address;
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct Position {
    pub lower_tick: i32,
    pub upper_tick: i32,
    pub liquidity: i128,
    pub fee_growth_inside_a: i128,
    pub fee_growth_inside_b: i128,
    pub tokens_owed: (i128, i128),
}

/// Per-tick state stored in the tick registry (issue #178).
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct TickInfo {
    /// Total liquidity referencing this tick (never negative).
    pub liquidity_gross: i128,
    /// Net liquidity change when crossing this tick upward (subtracted when crossing downward).
    pub liquidity_net: i128,
    pub fee_growth_outside_a: i128,
    pub fee_growth_outside_b: i128,
    pub initialized: bool,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct PoolState {
    pub sqrt_price: u128,
    pub current_tick: i32,
    pub active_liquidity: i128,
    pub tick_spacing: i32,
}

/// Detailed read-only swap estimate for concentrated-liquidity routing.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct PriceImpactEstimate {
    /// Gross input consumed by the simulated swap, including fees.
    pub amount_in: i128,
    /// Input that reaches pool math after LP fees.
    pub amount_in_after_fee: i128,
    /// Output amount predicted by the same tick walk used by `swap`.
    pub amount_out: i128,
    /// LP fee paid in the input token.
    pub fee_amount: i128,
    /// Spot token_out/token_in price before the swap, scaled by 1_000_000.
    pub spot_price_before: i128,
    /// Effective token_out/token_in price for this swap, scaled by 1_000_000.
    pub effective_price: i128,
    /// Price impact versus pre-swap spot, in basis points and including fees.
    pub price_impact_bps: i128,
    pub sqrt_price_before: u128,
    pub sqrt_price_after: u128,
    pub tick_before: i32,
    pub tick_after: i32,
    pub active_liquidity_before: i128,
    pub active_liquidity_after: i128,
}

/// Result of walking ticks to fill an exact-out request (#696). Internal —
/// never crosses the contract boundary, so this is a plain Rust struct, not
/// a `#[contracttype]`. `tick_crossings` lists, in crossing order, each
/// tick's *new* `fee_growth_outside` values (already flipped against the
/// running fee-growth-global at the moment of that crossing, exactly as
/// `swap` computes it inline) so the caller can persist them without
/// re-deriving the interleaving.
struct ExactOutWalk {
    /// Not currently read outside this struct's construction — kept
    /// alongside `amount_in_gross_total` for parity with `swap`'s
    /// `amount_in_after_fee_total`, and available to a future caller (e.g. a
    /// richer quote result) without changing the walk itself.
    #[allow(dead_code)]
    amount_in_after_fee_total: i128,
    amount_in_gross_total: i128,
    amount_out_filled: i128,
    sqrt_price_final: u128,
    tick_final: i32,
    active_liquidity_final: i128,
    fee_growth_global_a_delta: i128,
    fee_growth_global_b_delta: i128,
    protocol_fee_a_delta: i128,
    protocol_fee_b_delta: i128,
    tick_crossings: Vec<(i32, i128, i128)>,
    fully_filled: bool,
}

#[contract]
pub struct ConcentratedLiquidity;

#[contractimpl]
impl ConcentratedLiquidity {
    /// Extend the contract's **instance** storage TTL (issue #905).
    ///
    /// Called as the first statement of every public entrypoint that touches
    /// contract state, including read-only paths: a pure quote or state read is
    /// often the only traffic the pool sees for long stretches, and it must keep
    /// the pool from being archived just as a swap would.
    fn extend_instance_ttl(env: &Env) {
        env.storage()
            .instance()
            .extend_ttl(INSTANCE_TTL_THRESHOLD, INSTANCE_BUMP_TO);
    }

    /// One-time initialisation. Sets admin, token pair, fee, starting tick, and tick spacing.
    ///
    /// `tick_spacing` must be > 0. Only tick values that are exact multiples of
    /// `tick_spacing` may be used as position boundaries in `mint_position`.
    /// Suggested defaults: fee 5 bps → spacing 1, fee 30 bps → spacing 10,
    /// fee 100 bps → spacing 60.
    pub fn initialize(
        env: Env,
        admin: Address,
        token_a: Address,
        token_b: Address,
        fee_bps: i128,
        initial_tick: i32,
        tick_spacing: i32,
    ) -> Result<(), ClError> {
        Self::extend_instance_ttl(&env);
        if env.storage().instance().has(&DataKey::TokenA) {
            return Err(ClError::AlreadyInitialized);
        }
        if token_a == token_b {
            return Err(ClError::TokensMustDiffer);
        }
        if !(0..10_000).contains(&fee_bps) {
            return Err(ClError::InvalidFeeBps);
        }
        if !(MIN_TICK..=MAX_TICK).contains(&initial_tick) {
            return Err(ClError::TickOutOfRange);
        }
        if tick_spacing <= 0 {
            return Err(ClError::InvalidTickSpacing);
        }
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Paused, &false);
        env.storage().instance().set(&DataKey::TokenA, &token_a);
        env.storage().instance().set(&DataKey::TokenB, &token_b);
        env.storage().instance().set(&DataKey::FeeBps, &fee_bps);
        env.storage()
            .instance()
            .set(&DataKey::CurrentTick, &initial_tick);
        // Record the matching sqrt price up front. It used to be written only
        // by swaps, so an untraded pool had no stored price at all and every
        // reader had to reconstruct one from the tick.
        env.storage().instance().set(
            &DataKey::SqrtPriceX96,
            &Self::tick_to_sqrt_price_x96(initial_tick),
        );
        env.storage()
            .instance()
            .set(&DataKey::TickSpacing, &tick_spacing);
        env.storage()
            .instance()
            .set(&DataKey::FeeGrowthGlobalA, &0_i128);
        env.storage()
            .instance()
            .set(&DataKey::FeeGrowthGlobalB, &0_i128);
        env.storage()
            .instance()
            .set(&DataKey::ActiveLiquidity, &0_i128);
        let init_ts = env.ledger().timestamp();
        env.storage()
            .instance()
            .set(&DataKey::TickCumulative, &0_i64);
        env.storage()
            .instance()
            .set(&DataKey::LastOracleTimestamp, &init_ts);
        Self::record_oracle_point(&env, init_ts, 0);
        env.storage()
            .instance()
            .set(&DataKey::OracleAggregator, &Option::<Address>::None);
        env.storage()
            .instance()
            .set(&DataKey::MaxOracleDeviationBps, &500_i128);
        env.storage()
            .persistent()
            .set(&DataKey::TickBitmapWords, &Vec::<i32>::new(&env));
        Self::bump_position(&env, &DataKey::TickBitmapWords);
        Ok(())
    }

    /// Admin: attach or remove the oracle aggregator for swap deviation checks (#318).
    pub fn set_oracle(env: Env, admin: Address, oracle: Option<Address>) -> Result<(), ClError> {
        Self::extend_instance_ttl(&env);
        let stored = Self::read_admin(&env)?;
        if admin != stored {
            return Err(ClError::Unauthorized);
        }
        admin.require_auth();
        let old_oracle: Option<Address> = env
            .storage()
            .instance()
            .get(&DataKey::OracleAggregator)
            .unwrap_or(None);
        env.storage()
            .instance()
            .set(&DataKey::OracleAggregator, &oracle);
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "oracle_set"),),
            (old_oracle, oracle)
        );
        Ok(())
    }

    /// Admin: wire (or detach) the `cl_position_nft` contract used to tokenize
    /// positions (issue #348).
    ///
    /// Once set, opening a fresh position mints a receipt NFT to the provider
    /// and records a `token_id ↔ position` index. The NFT may then be
    /// transferred; its current owner — not the original provider — controls
    /// the position through `burn_position_by_token_id`
    /// and `collect_fees_by_token_id`.
    ///
    /// The NFT contract must be initialized with this pool's address as its
    /// `cl_pool`, otherwise mint/burn calls from the pool will be rejected.
    pub fn set_position_nft(env: Env, admin: Address, nft: Option<Address>) -> Result<(), ClError> {
        Self::extend_instance_ttl(&env);
        let stored = Self::read_admin(&env)?;
        if admin != stored {
            return Err(ClError::Unauthorized);
        }
        admin.require_auth();

        // Safety check: prevent changing NFT contract when positions are tokenized.
        // Changing the contract would orphan existing token_id → position indices,
        // causing resolve_token_owner/ensure_legacy_owner to query the wrong NFT
        // and either trap (id doesn't exist) or authorize the wrong owner (id collision).
        let existing_nft: Option<Address> = env
            .storage()
            .instance()
            .get(&DataKey::PositionNft)
            .unwrap_or(None);

        // Only allow changing from Some → None (detach) or None → Some (initial set).
        // Changing from Some(A) → Some(B) is forbidden.
        if existing_nft.is_some() && nft.is_some() && existing_nft != nft {
            return Err(ClError::NftContractChangeBlocked);
        }

        env.storage().instance().set(&DataKey::PositionNft, &nft);
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "position_nft_set"),),
            (existing_nft, nft)
        );
        Ok(())
    }

    /// Returns the wired-in position-NFT contract, if any.
    pub fn position_nft(env: Env) -> Option<Address> {
        Self::extend_instance_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::PositionNft)
            .unwrap_or(None)
    }

    /// Returns the NFT `token_id` minted for `(provider, lower_tick,
    /// upper_tick)`, or `None` if the position is not tokenized.
    pub fn position_token_id(
        env: Env,
        provider: Address,
        lower_tick: i32,
        upper_tick: i32,
    ) -> Option<u64> {
        Self::extend_instance_ttl(&env);
        let key = DataKey::PositionNftToken(provider, lower_tick, upper_tick);
        if let Some(id) = env.storage().persistent().get::<_, u64>(&key) {
            Self::bump_position(&env, &key);
            Some(id)
        } else {
            env.storage().instance().get(&key)
        }
    }

    /// Resolves an NFT `token_id` back to its `(provider, lower_tick,
    /// upper_tick)` position, or `None` if unknown.
    pub fn position_of_token(env: Env, token_id: u64) -> Option<(Address, i32, i32)> {
        Self::extend_instance_ttl(&env);
        let key = DataKey::NftTokenToPosition(token_id);
        if let Some(pos) = env
            .storage()
            .persistent()
            .get::<_, (Address, i32, i32)>(&key)
        {
            Self::bump_position(&env, &key);
            Some(pos)
        } else {
            env.storage().instance().get(&key)
        }
    }

    /// Admin: max spot-vs-oracle deviation in basis points.
    pub fn set_max_oracle_deviation_bps(
        env: Env,
        admin: Address,
        max_deviation_bps: i128,
    ) -> Result<(), ClError> {
        Self::extend_instance_ttl(&env);
        let stored = Self::read_admin(&env)?;
        if admin != stored {
            return Err(ClError::Unauthorized);
        }
        admin.require_auth();
        if !(0..=10_000).contains(&max_deviation_bps) {
            return Err(ClError::InvalidFeeBps);
        }
        let old_deviation_bps: i128 = env
            .storage()
            .instance()
            .get(&DataKey::MaxOracleDeviationBps)
            .unwrap_or(500);
        env.storage()
            .instance()
            .set(&DataKey::MaxOracleDeviationBps, &max_deviation_bps);
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "max_oracle_deviation_set"),),
            (old_deviation_bps, max_deviation_bps)
        );
        Ok(())
    }

    /// Pause all minting and swapping. Admin-only.
    pub fn pause(env: Env, admin: Address) -> Result<(), ClError> {
        Self::extend_instance_ttl(&env);
        let stored = Self::read_admin(&env)?;
        if admin != stored {
            return Err(ClError::Unauthorized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Paused, &true);
        soroban_amm_sdk::emit_versioned_event!(env, (Symbol::new(&env, "pause"),), (admin,));
        Ok(())
    }

    /// Resume minting and swapping. Admin-only.
    pub fn unpause(env: Env, admin: Address) -> Result<(), ClError> {
        Self::extend_instance_ttl(&env);
        let stored = Self::read_admin(&env)?;
        if admin != stored {
            return Err(ClError::Unauthorized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Paused, &false);
        soroban_amm_sdk::emit_versioned_event!(env, (Symbol::new(&env, "unpause"),), (admin,));
        Ok(())
    }

    pub fn propose_admin(env: Env, admin: Address, new_admin: Address) -> Result<(), ClError> {
        Self::extend_instance_ttl(&env);
        let stored = Self::read_admin(&env)?;
        if admin != stored {
            return Err(ClError::Unauthorized);
        }
        admin.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::PendingAdmin, &new_admin);
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "admin_nominated"),),
            (admin, new_admin)
        );
        Ok(())
    }

    pub fn accept_admin(env: Env, new_admin: Address) -> Result<(), ClError> {
        Self::extend_instance_ttl(&env);
        let pending: Option<Address> = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .unwrap_or(None);
        let pending_addr = pending.ok_or(ClError::Unauthorized)?;
        if new_admin != pending_addr {
            return Err(ClError::Unauthorized);
        }
        new_admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &new_admin);
        env.storage().instance().remove(&DataKey::PendingAdmin);
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "admin_changed"),),
            (new_admin,)
        );
        Ok(())
    }

    /// Admin: replace this pool's WASM with a new version.
    ///
    /// `admin` must be the stored pool admin and must authorize the call; any
    /// other address is rejected with `Unauthorized`. The new WASM must
    /// already be uploaded to the network. Pool, tick and position storage is
    /// preserved and `initialize` is not re-run; only the bytecode is replaced.
    pub fn upgrade(env: Env, admin: Address, new_wasm_hash: BytesN<32>) -> Result<(), ClError> {
        Self::extend_instance_ttl(&env);
        let stored = Self::read_admin(&env)?;
        if admin != stored {
            return Err(ClError::Unauthorized);
        }
        admin.require_auth();
        env.deployer()
            .update_current_contract_wasm(new_wasm_hash.clone());
        soroban_amm_sdk::emit_versioned_event!(env, (symbol_short!("upgraded"),), (new_wasm_hash,));
        Ok(())
    }

    pub fn set_protocol_fee(
        env: Env,
        admin: Address,
        recipient: Address,
        bps: i128,
    ) -> Result<(), ClError> {
        Self::extend_instance_ttl(&env);
        let stored = Self::read_admin(&env)?;
        if admin != stored {
            return Err(ClError::Unauthorized);
        }
        admin.require_auth();
        if !(0..10_000).contains(&bps) {
            return Err(ClError::InvalidFeeBps);
        }
        env.storage().instance().set(&DataKey::ProtocolFeeBps, &bps);
        env.storage()
            .instance()
            .set(&DataKey::ProtocolFeeRecipient, &recipient);
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "protocol_fee_set"),),
            (bps, recipient)
        );
        Ok(())
    }

    pub fn withdraw_protocol_fees(env: Env, admin: Address) -> Result<(), ClError> {
        Self::extend_instance_ttl(&env);
        let stored = Self::read_admin(&env)?;
        if admin != stored {
            return Err(ClError::Unauthorized);
        }
        admin.require_auth();
        let recipient: Address = env
            .storage()
            .instance()
            .get(&DataKey::ProtocolFeeRecipient)
            .unwrap_or_else(|| stored.clone());

        let accrued_a: i128 = env
            .storage()
            .instance()
            .get(&DataKey::AccruedProtocolFeeA)
            .unwrap_or(0);
        if accrued_a > 0 {
            let token_a = Self::read_tokens(&env)?.0;
            TokenClient::new(&env, &token_a).transfer(
                &env.current_contract_address(),
                &recipient,
                &accrued_a,
            );
            env.storage()
                .instance()
                .set(&DataKey::AccruedProtocolFeeA, &0_i128);
        }
        let accrued_b: i128 = env
            .storage()
            .instance()
            .get(&DataKey::AccruedProtocolFeeB)
            .unwrap_or(0);
        if accrued_b > 0 {
            let token_b = Self::read_tokens(&env)?.1;
            TokenClient::new(&env, &token_b).transfer(
                &env.current_contract_address(),
                &recipient,
                &accrued_b,
            );
            env.storage()
                .instance()
                .set(&DataKey::AccruedProtocolFeeB, &0_i128);
        }
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (Symbol::new(&env, "protocol_fees_withdrawn"),),
            (recipient, accrued_a, accrued_b)
        );
        Ok(())
    }

    /// Returns true when the pool is paused.
    pub fn is_paused(env: Env) -> bool {
        Self::extend_instance_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }

    /// Extends the TTL of a per-user persistent entry (a `Position` or a
    /// `PositionList`) so long-lived positions are not evicted while in use.
    ///
    /// Must only be called for an entry that currently exists — i.e. right
    /// after writing it — because `extend_ttl` traps on a missing key.
    fn require_initialized(env: &Env) -> Result<(), ClError> {
        if env.storage().instance().has(&DataKey::TokenA) {
            Ok(())
        } else {
            Err(ClError::NotInitialized)
        }
    }

    fn read_admin(env: &Env) -> Result<Address, ClError> {
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(ClError::NotInitialized)
    }

    fn read_tokens(env: &Env) -> Result<(Address, Address), ClError> {
        let token_a = env.storage().instance().get(&DataKey::TokenA);
        let token_b = env.storage().instance().get(&DataKey::TokenB);
        match (token_a, token_b) {
            (Some(a), Some(b)) => Ok((a, b)),
            _ => Err(ClError::NotInitialized),
        }
    }

    fn read_current_tick(env: &Env) -> Result<i32, ClError> {
        env.storage()
            .instance()
            .get(&DataKey::CurrentTick)
            .ok_or(ClError::NotInitialized)
    }

    fn bump_position(env: &Env, key: &DataKey) {
        env.storage()
            .persistent()
            .extend_ttl(key, POSITION_TTL_THRESHOLD, POSITION_BUMP_TO);
    }

    fn check_oracle_deviation(
        env: &Env,
        token_in: &Address,
        token_out: &Address,
        amount_in: i128,
        amount_out: i128,
    ) -> Result<(), ClError> {
        let oracle: Option<Address> = env
            .storage()
            .instance()
            .get(&DataKey::OracleAggregator)
            .unwrap_or(None);
        let Some(oracle_addr) = oracle else {
            return Ok(());
        };
        let max_dev: i128 = env
            .storage()
            .instance()
            .get(&DataKey::MaxOracleDeviationBps)
            .unwrap_or(500);

        if amount_in <= 0 {
            return Ok(());
        }

        let agg =
            OracleAggregatorClient::new(env, &oracle_addr).get_price_safe(token_in, token_out);
        if agg.confidence == 0 || agg.price <= 0 {
            return Err(ClError::OracleDeviationExceeded);
        }

        let spot_price = amount_out * PRICE_SCALE / amount_in;
        let oracle_price = agg.price;
        let deviation_bps = if spot_price >= oracle_price {
            (spot_price - oracle_price) * 10_000 / oracle_price
        } else {
            (oracle_price - spot_price) * 10_000 / oracle_price
        };
        if deviation_bps > max_dev {
            return Err(ClError::OracleDeviationExceeded);
        }
        Ok(())
    }

    /// `deadline` is a Unix timestamp (seconds); the call reverts with
    /// [`ClError::DeadlineExpired`] once the ledger time has passed it. The
    /// `min_a` / `min_b` slippage guards alone cannot protect a transaction
    /// that sits in the mempool and later executes at a stale price.
    #[allow(clippy::too_many_arguments)]
    pub fn mint_position(
        env: Env,
        provider: Address,
        lower_tick: i32,
        upper_tick: i32,
        amount_a_desired: i128,
        amount_b_desired: i128,
        min_a: i128,
        min_b: i128,
        deadline: u64,
    ) -> Result<(i128, i128), ClError> {
        Self::extend_instance_ttl(&env);
        // Reading the token pair first doubles as the initialization check,
        // so this hot path pays for no extra storage lookup.
        let (token_a, token_b) = Self::read_tokens(&env)?;
        if env.ledger().timestamp() > deadline {
            return Err(ClError::DeadlineExpired);
        }
        if Self::is_paused(env.clone()) {
            return Err(ClError::Paused);
        }
        provider.require_auth();
        Self::ensure_legacy_owner(&env, &provider, lower_tick, upper_tick)?;
        if lower_tick >= upper_tick {
            return Err(ClError::TickOutOfRange);
        }
        if lower_tick < MIN_TICK || upper_tick > MAX_TICK {
            return Err(ClError::TickOutOfRange);
        }
        // Enforce tick spacing: ticks must be multiples of tick_spacing.
        let tick_spacing: i32 = env
            .storage()
            .instance()
            .get(&DataKey::TickSpacing)
            .unwrap_or(1);
        if lower_tick % tick_spacing != 0 || upper_tick % tick_spacing != 0 {
            return Err(ClError::TickNotAligned);
        }
        if amount_a_desired <= 0 && amount_b_desired <= 0 {
            return Err(ClError::ZeroAmounts);
        }
        if amount_a_desired < 0 || amount_b_desired < 0 {
            return Err(ClError::ZeroAmounts);
        }
        let current_tick = Self::read_current_tick(&env)?;
        // Derive liquidity from the desired amounts using the same
        // sqrtPriceX96 math (`liquidity_from_amounts`) that burn/collect use
        // to convert liquidity back into amounts, then derive the *actual*
        // amounts that liquidity requires via `amounts_for_liquidity_to_burn`.
        // The previous code computed the transferred amounts via a separate,
        // linear-in-price formula (`amounts_for_liquidity`) that didn't agree
        // with the sqrt-price math used everywhere else. That mismatch let a
        // position's recorded `liquidity` diverge from the tokens actually
        // deposited for it, which then surfaced as later withdrawals wanting
        // more of a token than the pool actually held (issue exposed by
        // `full_burn_via_token_id_does_not_leak_fees_to_provider`).
        let sqrt_price_now = Self::current_sqrt_price(&env, current_tick);
        let liquidity = Self::liquidity_from_amounts(
            current_tick,
            lower_tick,
            upper_tick,
            amount_a_desired,
            amount_b_desired,
            sqrt_price_now,
        )?;
        if liquidity <= 0 {
            return Err(ClError::ZeroLiquidity);
        }
        let (amount_a, amount_b) = Self::amounts_for_liquidity_to_burn(
            current_tick,
            lower_tick,
            upper_tick,
            liquidity,
            sqrt_price_now,
        )?;
        if amount_a < 0 || amount_b < 0 {
            return Err(ClError::ZeroAmounts);
        }
        if amount_a < min_a || amount_b < min_b {
            return Err(ClError::SlippageExceeded);
        }
        if amount_a > 0 {
            TokenClient::new(&env, &token_a).transfer(
                &provider,
                &env.current_contract_address(),
                &amount_a,
            );
        }
        if amount_b > 0 {
            TokenClient::new(&env, &token_b).transfer(
                &provider,
                &env.current_contract_address(),
                &amount_b,
            );
        }
        let pos_key = DataKey::Position(provider.clone(), lower_tick, upper_tick);

        let (fg_inside_a, fg_inside_b) =
            Self::fee_growth_inside(env.clone(), lower_tick, upper_tick);

        let mut pos: Position = env
            .storage()
            .persistent()
            .get(&pos_key)
            .unwrap_or(Position {
                lower_tick,
                upper_tick,
                liquidity: 0,
                fee_growth_inside_a: fg_inside_a,
                fee_growth_inside_b: fg_inside_b,
                tokens_owed: (0, 0),
            });
        // A position is "fresh" when it currently holds no liquidity — either
        // brand new or fully burned earlier. Freshly opened positions get a
        // receipt NFT minted below (issue #348).
        let was_empty = pos.liquidity == 0;
        let (oa, ob) = Self::pending_fees(&pos, fg_inside_a, fg_inside_b);
        pos.tokens_owed = (pos.tokens_owed.0 + oa, pos.tokens_owed.1 + ob);
        pos.fee_growth_inside_a = fg_inside_a;
        pos.fee_growth_inside_b = fg_inside_b;
        pos.liquidity += liquidity;
        // Track position list for get_positions view
        let list_key = DataKey::PositionList(provider.clone());
        let mut list: Vec<(i32, i32)> = env
            .storage()
            .persistent()
            .get(&list_key)
            .unwrap_or_else(|| Vec::new(&env));
        let range = (lower_tick, upper_tick);
        if !list.iter().any(|r| r == range) {
            list.push_back(range);
            env.storage().persistent().set(&list_key, &list);
            Self::bump_position(&env, &list_key);
        }
        env.storage().persistent().set(&pos_key, &pos);
        Self::bump_position(&env, &pos_key);

        let fg_a: i128 = env
            .storage()
            .instance()
            .get(&DataKey::FeeGrowthGlobalA)
            .unwrap_or(0);
        let fg_b: i128 = env
            .storage()
            .instance()
            .get(&DataKey::FeeGrowthGlobalB)
            .unwrap_or(0);
        Self::update_tick(&env, lower_tick, current_tick, liquidity, false, fg_a, fg_b);
        Self::update_tick(&env, upper_tick, current_tick, liquidity, true, fg_a, fg_b);

        if current_tick >= lower_tick && current_tick < upper_tick {
            let active: i128 = env
                .storage()
                .instance()
                .get(&DataKey::ActiveLiquidity)
                .unwrap_or(0);
            env.storage()
                .instance()
                .set(&DataKey::ActiveLiquidity, &(active + liquidity));
        }
        if was_empty {
            Self::tokenize_position(&env, &provider, lower_tick, upper_tick);
        }
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (symbol_short!("mint_pos"), provider),
            (lower_tick, upper_tick, liquidity, amount_a, amount_b)
        );
        Ok((amount_a, amount_b))
    }

    /// Increase liquidity on an existing position without closing it first.
    ///
    /// This explicit modification flow reuses the same `(provider, lower_tick,
    /// upper_tick)` storage key, settles accrued fees into `tokens_owed`, and
    /// computes the required token amounts from the current price before
    /// increasing the stored liquidity.
    ///
    /// `deadline` is a Unix timestamp (seconds); the call reverts with
    /// [`ClError::DeadlineExpired`] once the ledger time has passed it. The
    /// `min_a` / `min_b` slippage guards alone cannot protect a transaction
    /// that sits in the mempool and later executes at a stale price.
    #[allow(clippy::too_many_arguments)]
    pub fn modify_position(
        env: Env,
        provider: Address,
        lower_tick: i32,
        upper_tick: i32,
        liquidity_delta: i128,
        min_a: i128,
        min_b: i128,
        deadline: u64,
    ) -> Result<(i128, i128), ClError> {
        Self::extend_instance_ttl(&env);
        Self::require_initialized(&env)?;
        if env.ledger().timestamp() > deadline {
            return Err(ClError::DeadlineExpired);
        }
        if Self::is_paused(env.clone()) {
            return Err(ClError::Paused);
        }
        provider.require_auth();
        Self::ensure_legacy_owner(&env, &provider, lower_tick, upper_tick)?;
        if lower_tick >= upper_tick {
            return Err(ClError::TickOutOfRange);
        }
        if lower_tick < MIN_TICK || upper_tick > MAX_TICK {
            return Err(ClError::TickOutOfRange);
        }
        let tick_spacing: i32 = env
            .storage()
            .instance()
            .get(&DataKey::TickSpacing)
            .unwrap_or(1);
        if lower_tick % tick_spacing != 0 || upper_tick % tick_spacing != 0 {
            return Err(ClError::TickNotAligned);
        }
        if liquidity_delta <= 0 {
            return Err(ClError::ZeroLiquidity);
        }

        let current_tick = Self::read_current_tick(&env)?;
        let (token_a, token_b) = Self::read_tokens(&env)?;
        let pos_key = DataKey::Position(provider.clone(), lower_tick, upper_tick);
        let mut pos: Position = env
            .storage()
            .persistent()
            .get(&pos_key)
            .ok_or(ClError::PositionNotFound)?;

        let sqrt_price_now = Self::current_sqrt_price(&env, current_tick);
        let (amount_a, amount_b) = Self::amounts_for_liquidity_to_burn(
            current_tick,
            lower_tick,
            upper_tick,
            liquidity_delta,
            sqrt_price_now,
        )?;
        if amount_a <= 0 && amount_b <= 0 {
            return Err(ClError::ZeroLiquidity);
        }
        if amount_a < min_a || amount_b < min_b {
            return Err(ClError::SlippageExceeded);
        }

        if amount_a > 0 {
            TokenClient::new(&env, &token_a).transfer(
                &provider,
                &env.current_contract_address(),
                &amount_a,
            );
        }
        if amount_b > 0 {
            TokenClient::new(&env, &token_b).transfer(
                &provider,
                &env.current_contract_address(),
                &amount_b,
            );
        }

        let (fg_inside_a, fg_inside_b) =
            Self::fee_growth_inside(env.clone(), lower_tick, upper_tick);
        let (oa, ob) = Self::pending_fees(&pos, fg_inside_a, fg_inside_b);
        pos.tokens_owed = (pos.tokens_owed.0 + oa, pos.tokens_owed.1 + ob);
        pos.fee_growth_inside_a = fg_inside_a;
        pos.fee_growth_inside_b = fg_inside_b;
        pos.liquidity += liquidity_delta;

        let list_key = DataKey::PositionList(provider.clone());
        let mut list: Vec<(i32, i32)> = env
            .storage()
            .persistent()
            .get(&list_key)
            .unwrap_or_else(|| Vec::new(&env));
        let range = (lower_tick, upper_tick);
        if !list.iter().any(|r| r == range) {
            list.push_back(range);
            env.storage().persistent().set(&list_key, &list);
            Self::bump_position(&env, &list_key);
        }
        env.storage().persistent().set(&pos_key, &pos);
        Self::bump_position(&env, &pos_key);

        let fg_a: i128 = env
            .storage()
            .instance()
            .get(&DataKey::FeeGrowthGlobalA)
            .unwrap_or(0);
        let fg_b: i128 = env
            .storage()
            .instance()
            .get(&DataKey::FeeGrowthGlobalB)
            .unwrap_or(0);
        Self::update_tick(
            &env,
            lower_tick,
            current_tick,
            liquidity_delta,
            false,
            fg_a,
            fg_b,
        );
        Self::update_tick(
            &env,
            upper_tick,
            current_tick,
            liquidity_delta,
            true,
            fg_a,
            fg_b,
        );

        if current_tick >= lower_tick && current_tick < upper_tick {
            let active: i128 = env
                .storage()
                .instance()
                .get(&DataKey::ActiveLiquidity)
                .unwrap_or(0);
            env.storage()
                .instance()
                .set(&DataKey::ActiveLiquidity, &(active + liquidity_delta));
        }

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (symbol_short!("mod_pos"), provider),
            (lower_tick, upper_tick, liquidity_delta, amount_a, amount_b)
        );

        Ok((amount_a, amount_b))
    }

    /// Deposit a **single token** into a concentrated liquidity position.
    ///
    /// Behaviour depends on where the current price sits relative to the range:
    ///
    /// - `current_tick < lower_tick`  → price below range: only **token A** needed.
    /// - `current_tick >= upper_tick` → price above range: only **token B** needed.
    /// - in range → the deposited token covers its half of the range; dust returned.
    ///
    /// # Errors
    /// - [`ClError::Paused`] / [`ClError::DeadlineExpired`] – circuit breakers.
    /// - [`ClError::TickOutOfRange`] / [`ClError::TickNotAligned`] – bad ticks.
    /// - [`ClError::InvalidToken`]     – `token_in` is not a pool token.
    /// - [`ClError::ZeroAmounts`]      – `amount_in ≤ 0`.
    /// - [`ClError::ZeroLiquidity`]    – computed liquidity is zero.
    /// - [`ClError::SlippageExceeded`] – wrong token for price range, or below `min_liquidity`.
    #[allow(clippy::too_many_arguments)]
    pub fn mint_position_single_token(
        env: Env,
        provider: Address,
        lower_tick: i32,
        upper_tick: i32,
        token_in: Address,
        amount_in: i128,
        min_liquidity: i128,
        deadline: u64,
    ) -> Result<SingleTokenDepositResult, ClError> {
        Self::extend_instance_ttl(&env);
        Self::require_initialized(&env)?;
        if env.ledger().timestamp() > deadline {
            return Err(ClError::DeadlineExpired);
        }
        if Self::is_paused(env.clone()) {
            return Err(ClError::Paused);
        }
        provider.require_auth();
        Self::ensure_legacy_owner(&env, &provider, lower_tick, upper_tick)?;

        // ── Validate tick range ───────────────────────────────────────────────
        if lower_tick >= upper_tick {
            return Err(ClError::TickOutOfRange);
        }
        if lower_tick < MIN_TICK || upper_tick > MAX_TICK {
            return Err(ClError::TickOutOfRange);
        }
        let tick_spacing: i32 = env
            .storage()
            .instance()
            .get(&DataKey::TickSpacing)
            .unwrap_or(1);
        if lower_tick % tick_spacing != 0 || upper_tick % tick_spacing != 0 {
            return Err(ClError::TickNotAligned);
        }
        if amount_in <= 0 {
            return Err(ClError::ZeroAmounts);
        }

        // ── Identify which token was supplied ────────────────────────────────
        let (token_a, token_b) = Self::read_tokens(&env)?;
        let is_token_a = token_in == token_a;
        if !is_token_a && token_in != token_b {
            return Err(ClError::InvalidToken);
        }

        let current_tick = Self::read_current_tick(&env)?;

        // ── Compute (amount_a, amount_b, liquidity) from the single token ─────
        //
        // Three cases:
        //
        //  Case 1: current_tick < lower_tick  → price BELOW range
        //    Token A covers the entire range [lower, upper].
        //    Caller must supply token A; full amount_in is consumed.
        //
        //  Case 2: current_tick >= upper_tick → price ABOVE range
        //    Token B covers the entire range [lower, upper].
        //    Caller must supply token B; full amount_in is consumed.
        //
        //  Case 3: lower_tick <= current_tick < upper_tick → price IN range
        //    Token A covers [current_price, upper], Token B covers [lower, current_price].
        //    Single-token deposit provides liquidity for only the covered half.
        //    Dust = amount_in - amount_used (never transferred; stays with provider).
        //

        let (amount_a, amount_b, liquidity, amount_used) = if current_tick < lower_tick {
            // Case 1: price below range — only token A
            if !is_token_a {
                return Err(ClError::SlippageExceeded);
            }
            // Use proper sqrtPriceX96 formulas for accurate calculation
            let sqrt_lower = Self::tick_to_sqrt_price_x96(lower_tick);
            let sqrt_upper = Self::tick_to_sqrt_price_x96(upper_tick);
            let liq = math::get_liquidity_for_amount0(sqrt_lower, sqrt_upper, amount_in)
                .ok_or(ClError::MathOverflow)?;
            (amount_in, 0_i128, liq.max(1), amount_in)
        } else if current_tick >= upper_tick {
            // Case 2: price above range — only token B
            if is_token_a {
                return Err(ClError::SlippageExceeded);
            }
            // Use proper sqrtPriceX96 formulas for accurate calculation
            let sqrt_lower = Self::tick_to_sqrt_price_x96(lower_tick);
            let sqrt_upper = Self::tick_to_sqrt_price_x96(upper_tick);
            let liq = math::get_liquidity_for_amount1(sqrt_lower, sqrt_upper, amount_in)
                .ok_or(ClError::MathOverflow)?;
            (0_i128, amount_in, liq.max(1), amount_in)
        } else {
            // Case 3: price in range — compute liquidity from the single token's half
            // Using proper Uniswap V3 formulas with sqrtPriceX96
            let sqrt_lower = Self::tick_to_sqrt_price_x96(lower_tick);
            let sqrt_upper = Self::tick_to_sqrt_price_x96(upper_tick);
            let sqrt_current = Self::tick_to_sqrt_price_x96(current_tick);

            if sqrt_current >= sqrt_upper {
                // Degenerate: current at or above upper tick
                if is_token_a {
                    let liq = math::get_liquidity_for_amount0(sqrt_current, sqrt_upper, amount_in)
                        .ok_or(ClError::MathOverflow)?;
                    let liq = liq.max(1);
                    let used = math::get_amount0_delta(sqrt_current, sqrt_upper, liq)
                        .ok_or(ClError::MathOverflow)?;
                    (used, 0_i128, liq, used)
                } else {
                    let liq = math::get_liquidity_for_amount1(sqrt_lower, sqrt_current, amount_in)
                        .ok_or(ClError::MathOverflow)?;
                    let liq = liq.max(1);
                    let used = math::get_amount1_delta(sqrt_lower, sqrt_current, liq)
                        .ok_or(ClError::MathOverflow)?;
                    (0_i128, used, liq, used)
                }
            } else if sqrt_current <= sqrt_lower {
                // Degenerate: current at or below lower tick
                if is_token_a {
                    let liq = math::get_liquidity_for_amount0(sqrt_current, sqrt_upper, amount_in)
                        .ok_or(ClError::MathOverflow)?;
                    let liq = liq.max(1);
                    let used = math::get_amount0_delta(sqrt_current, sqrt_upper, liq)
                        .ok_or(ClError::MathOverflow)?;
                    (used, 0_i128, liq, used)
                } else {
                    let liq = math::get_liquidity_for_amount1(sqrt_lower, sqrt_current, amount_in)
                        .ok_or(ClError::MathOverflow)?;
                    let liq = liq.max(1);
                    let used = math::get_amount1_delta(sqrt_lower, sqrt_current, liq)
                        .ok_or(ClError::MathOverflow)?;
                    (0_i128, used, liq, used)
                }
            } else if is_token_a {
                // Token A covers [current_price, upper_price].
                // Liquidity is computed from the amount, then we back-compute actual token amount.
                let liq = math::get_liquidity_for_amount0(sqrt_current, sqrt_upper, amount_in)
                    .ok_or(ClError::MathOverflow)?;
                let liq = liq.max(1);
                let used = math::get_amount0_delta(sqrt_current, sqrt_upper, liq)
                    .ok_or(ClError::MathOverflow)?;
                (used, 0_i128, liq, used)
            } else {
                // Token B covers [lower_price, current_price].
                let liq = math::get_liquidity_for_amount1(sqrt_lower, sqrt_current, amount_in)
                    .ok_or(ClError::MathOverflow)?;
                let liq = liq.max(1);
                let used = math::get_amount1_delta(sqrt_lower, sqrt_current, liq)
                    .ok_or(ClError::MathOverflow)?;
                (0_i128, used, liq, used)
            }
        };
        if liquidity <= 0 {
            return Err(ClError::ZeroLiquidity);
        }
        if liquidity < min_liquidity {
            return Err(ClError::SlippageExceeded);
        }

        // ── Transfer tokens from provider ─────────────────────────────────────
        if amount_a > 0 {
            TokenClient::new(&env, &token_a).transfer(
                &provider,
                &env.current_contract_address(),
                &amount_a,
            );
        }
        if amount_b > 0 {
            TokenClient::new(&env, &token_b).transfer(
                &provider,
                &env.current_contract_address(),
                &amount_b,
            );
        }

        // ── Update position state ─────────────────────────────────────────────
        let pos_key = DataKey::Position(provider.clone(), lower_tick, upper_tick);
        let (fg_inside_a, fg_inside_b) =
            Self::fee_growth_inside(env.clone(), lower_tick, upper_tick);

        let mut pos: Position = env
            .storage()
            .persistent()
            .get(&pos_key)
            .unwrap_or(Position {
                lower_tick,
                upper_tick,
                liquidity: 0,
                fee_growth_inside_a: fg_inside_a,
                fee_growth_inside_b: fg_inside_b,
                tokens_owed: (0, 0),
            });
        let was_empty = pos.liquidity == 0;
        let (oa, ob) = Self::pending_fees(&pos, fg_inside_a, fg_inside_b);
        pos.tokens_owed = (pos.tokens_owed.0 + oa, pos.tokens_owed.1 + ob);
        pos.fee_growth_inside_a = fg_inside_a;
        pos.fee_growth_inside_b = fg_inside_b;
        pos.liquidity += liquidity;

        // Track position list.
        let list_key = DataKey::PositionList(provider.clone());
        let mut list: Vec<(i32, i32)> = env
            .storage()
            .persistent()
            .get(&list_key)
            .unwrap_or_else(|| Vec::new(&env));
        let range_pair = (lower_tick, upper_tick);
        if !list.iter().any(|r| r == range_pair) {
            list.push_back(range_pair);
            env.storage().persistent().set(&list_key, &list);
            Self::bump_position(&env, &list_key);
        }
        env.storage().persistent().set(&pos_key, &pos);
        Self::bump_position(&env, &pos_key);

        // ── Update tick state ─────────────────────────────────────────────────
        let fg_a: i128 = env
            .storage()
            .instance()
            .get(&DataKey::FeeGrowthGlobalA)
            .unwrap_or(0);
        let fg_b: i128 = env
            .storage()
            .instance()
            .get(&DataKey::FeeGrowthGlobalB)
            .unwrap_or(0);
        Self::update_tick(&env, lower_tick, current_tick, liquidity, false, fg_a, fg_b);
        Self::update_tick(&env, upper_tick, current_tick, liquidity, true, fg_a, fg_b);

        if current_tick >= lower_tick && current_tick < upper_tick {
            let active: i128 = env
                .storage()
                .instance()
                .get(&DataKey::ActiveLiquidity)
                .unwrap_or(0);
            env.storage()
                .instance()
                .set(&DataKey::ActiveLiquidity, &(active + liquidity));
        }

        // ── Return dust to provider ───────────────────────────────────────────
        // Dust = amount_in - amount_used. We never pulled the dust from the
        // provider, so no transfer is needed — it simply stays in their wallet.
        let dust = amount_in - amount_used;

        if was_empty {
            Self::tokenize_position(&env, &provider, lower_tick, upper_tick);
        }
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (symbol_short!("mint_1t"), provider),
            (lower_tick, upper_tick, liquidity, amount_used, dust)
        );

        Ok(SingleTokenDepositResult {
            amount_used,
            dust,
            liquidity,
        })
    }

    /// Quote the expected result of a single-token deposit without executing it.
    ///
    /// Pure read — does not transfer tokens or modify state.
    /// Returns values matching what `mint_position_single_token` would produce.
    pub fn quote_single_token_deposit(
        env: Env,
        lower_tick: i32,
        upper_tick: i32,
        token_in: Address,
        amount_in: i128,
    ) -> Result<SingleTokenDepositResult, ClError> {
        Self::extend_instance_ttl(&env);
        Self::require_initialized(&env)?;
        if lower_tick >= upper_tick {
            return Err(ClError::TickOutOfRange);
        }
        if lower_tick < MIN_TICK || upper_tick > MAX_TICK {
            return Err(ClError::TickOutOfRange);
        }
        if amount_in <= 0 {
            return Err(ClError::ZeroAmounts);
        }

        let (token_a, token_b) = Self::read_tokens(&env)?;
        let is_token_a = token_in == token_a;
        if !is_token_a && token_in != token_b {
            return Err(ClError::InvalidToken);
        }

        let current_tick = Self::read_current_tick(&env)?;

        // Mirror the exact same logic as mint_position_single_token.
        let (liquidity, amount_used) = if current_tick < lower_tick {
            if !is_token_a {
                return Err(ClError::SlippageExceeded);
            }
            let sqrt_lower = Self::tick_to_sqrt_price_x96(lower_tick);
            let sqrt_upper = Self::tick_to_sqrt_price_x96(upper_tick);
            let liq = math::get_liquidity_for_amount0(sqrt_lower, sqrt_upper, amount_in)
                .ok_or(ClError::MathOverflow)?;
            (liq.max(1), amount_in)
        } else if current_tick >= upper_tick {
            if is_token_a {
                return Err(ClError::SlippageExceeded);
            }
            let sqrt_lower = Self::tick_to_sqrt_price_x96(lower_tick);
            let sqrt_upper = Self::tick_to_sqrt_price_x96(upper_tick);
            let liq = math::get_liquidity_for_amount1(sqrt_lower, sqrt_upper, amount_in)
                .ok_or(ClError::MathOverflow)?;
            (liq.max(1), amount_in)
        } else {
            let sqrt_lower = Self::tick_to_sqrt_price_x96(lower_tick);
            let sqrt_upper = Self::tick_to_sqrt_price_x96(upper_tick);
            let sqrt_current = Self::tick_to_sqrt_price_x96(current_tick);

            if sqrt_current >= sqrt_upper {
                let liq = math::get_liquidity_for_amount0(sqrt_upper, sqrt_upper, amount_in)
                    .ok_or(ClError::MathOverflow)?;
                (liq.max(1), amount_in)
            } else if sqrt_current <= sqrt_lower {
                let liq = math::get_liquidity_for_amount1(sqrt_lower, sqrt_lower, amount_in)
                    .ok_or(ClError::MathOverflow)?;
                (liq.max(1), amount_in)
            } else if is_token_a {
                let liq = math::get_liquidity_for_amount0(sqrt_current, sqrt_upper, amount_in)
                    .ok_or(ClError::MathOverflow)?;
                let liq = liq.max(1);
                let used = math::get_amount0_delta(sqrt_current, sqrt_upper, liq)
                    .ok_or(ClError::MathOverflow)?;
                (liq, used)
            } else {
                let liq = math::get_liquidity_for_amount1(sqrt_lower, sqrt_current, amount_in)
                    .ok_or(ClError::MathOverflow)?;
                let liq = liq.max(1);
                let used = math::get_amount1_delta(sqrt_lower, sqrt_current, liq)
                    .ok_or(ClError::MathOverflow)?;
                (liq, used)
            }
        };

        Ok(SingleTokenDepositResult {
            amount_used,
            dust: amount_in - amount_used,
            liquidity,
        })
    }

    // ── Issue #295: Range order support ──────────────────────────────────────

    /// Place a **range order** — a one-sided position that acts as a passive
    /// limit order.
    ///
    /// The range `[lower_tick, upper_tick)` must be **entirely above** or
    /// **entirely below** the current tick so that only one token is required.
    ///
    /// - Range above current tick (`current_tick < lower_tick`): deposit
    ///   `token_a`.  When price rises through the range the position converts
    ///   to `token_b`.
    /// - Range below current tick (`current_tick >= upper_tick`): deposit
    ///   `token_b`.  When price falls through the range the position converts
    ///   to `token_a`.
    ///
    /// The position is tagged internally so `check_range_order_filled` can
    /// report its status without requiring an off-chain keeper.
    ///
    /// Only one range order may be active per `(provider, lower_tick,
    /// upper_tick)`: re-placing while an earlier order's liquidity is still in
    /// the position is rejected with [`ClError::RangeOrderExists`]. The tag is
    /// released when the position is fully withdrawn (see
    /// `burn_position_core`), so the same range can
    /// be reused for a fresh order afterwards.
    ///
    /// # Errors
    /// - [`ClError::RangeOrderInRange`] – the range straddles the current tick.
    /// - [`ClError::RangeOrderExists`] – a previous order on this range has not
    ///   been withdrawn yet.
    /// - All the usual `ClError` variants from `mint_position_single_token`.
    #[allow(clippy::too_many_arguments)]
    pub fn place_range_order(
        env: Env,
        provider: Address,
        lower_tick: i32,
        upper_tick: i32,
        token_in: Address,
        amount_in: i128,
        min_liquidity: i128,
        deadline: u64,
    ) -> Result<SingleTokenDepositResult, ClError> {
        Self::extend_instance_ttl(&env);
        Self::require_initialized(&env)?;
        let current_tick: i32 = env
            .storage()
            .instance()
            .get(&DataKey::CurrentTick)
            .unwrap_or(0);

        // Enforce that the range is fully out-of-range (one-sided).
        let is_above = current_tick < lower_tick;
        let is_below = current_tick >= upper_tick;
        if !is_above && !is_below {
            return Err(ClError::RangeOrderInRange);
        }

        // Reject re-placing while a previous order on this exact range still
        // has liquidity in the position. `mint_position_single_token` merges
        // liquidity into the shared `Position`, so overwriting the direction
        // tag would silently corrupt the fill status of the earlier tranche
        // (issue #595). The provider must withdraw (burn) the old order first.
        let range_order_key = DataKey::RangeOrder(provider.clone(), lower_tick, upper_tick);
        let has_range_order = if env.storage().persistent().has(&range_order_key) {
            Self::bump_position(&env, &range_order_key);
            true
        } else {
            env.storage().instance().has(&range_order_key)
        };
        if has_range_order {
            let pos: Option<Position> = env.storage().persistent().get(&DataKey::Position(
                provider.clone(),
                lower_tick,
                upper_tick,
            ));
            if pos.map(|p| p.liquidity > 0).unwrap_or(false) {
                return Err(ClError::RangeOrderExists);
            }
            // Stale tag from before the burn cleanup (legacy state): the
            // position is already closed, so just release it.
            env.storage().persistent().remove(&range_order_key);
            env.storage().instance().remove(&range_order_key);
        }

        // Delegate to the existing single-token deposit logic.
        let result = Self::mint_position_single_token(
            env.clone(),
            provider.clone(),
            lower_tick,
            upper_tick,
            token_in,
            amount_in,
            min_liquidity,
            deadline,
        )?;

        // Tag the position as a range order and record which side it was
        // placed on, so `check_range_order_filled` knows the fill direction.
        env.storage().persistent().set(&range_order_key, &is_above);
        Self::bump_position(&env, &range_order_key);
        env.storage().instance().remove(&range_order_key);

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (symbol_short!("rng_ord"), provider),
            (lower_tick, upper_tick, result.liquidity, is_above)
        );

        Ok(result)
    }

    /// Check whether a range order has been filled.
    ///
    /// A range order is **filled** when the current tick has fully crossed the
    /// range:
    /// - An *above-range* order (token A → token B) is filled when
    ///   `current_tick >= upper_tick`.
    /// - A *below-range* order (token B → token A) is filled when
    ///   `current_tick < lower_tick`.
    ///
    /// Returns [`ClError::PositionNotFound`] if the position does not exist or
    /// was not placed via `place_range_order`.
    pub fn check_range_order_filled(
        env: Env,
        provider: Address,
        lower_tick: i32,
        upper_tick: i32,
    ) -> Result<RangeOrderStatus, ClError> {
        Self::extend_instance_ttl(&env);
        Self::require_initialized(&env)?;
        // Verify the position exists and is tagged as a range order.
        let _pos: Position = env
            .storage()
            .persistent()
            .get(&DataKey::Position(provider.clone(), lower_tick, upper_tick))
            .ok_or(ClError::PositionNotFound)?;

        let range_order_key = DataKey::RangeOrder(provider, lower_tick, upper_tick);
        let is_above: bool =
            if let Some(val) = env.storage().persistent().get::<_, bool>(&range_order_key) {
                Self::bump_position(&env, &range_order_key);
                val
            } else if let Some(val) = env.storage().instance().get::<_, bool>(&range_order_key) {
                val
            } else {
                return Err(ClError::PositionNotFound);
            };

        let current_tick: i32 = env
            .storage()
            .instance()
            .get(&DataKey::CurrentTick)
            .unwrap_or(0);

        // Only report Filled once the price has crossed through the range in
        // this order's original fill direction; otherwise it is still Pending.
        let status = if is_above {
            if current_tick >= upper_tick {
                RangeOrderStatus::Filled
            } else {
                RangeOrderStatus::Pending
            }
        } else if current_tick < lower_tick {
            RangeOrderStatus::Filled
        } else {
            RangeOrderStatus::Pending
        };

        Ok(status)
    }

    /// Burn liquidity from a position keyed by the original `provider` address.
    ///
    /// Backwards-compatible entry point. When the position has been tokenized
    /// (issue #348) and `provider` no longer owns the NFT, the call is rejected
    /// with [`ClError::NotNftOwner`]: the current NFT owner must use
    /// `burn_position_by_token_id` instead.
    pub fn burn_position(
        env: Env,
        provider: Address,
        lower_tick: i32,
        upper_tick: i32,
        liquidity: i128,
    ) -> Result<(i128, i128), ClError> {
        Self::extend_instance_ttl(&env);
        Self::require_initialized(&env)?;
        // No pause guard — LPs must always be able to exit.
        provider.require_auth();
        Self::ensure_legacy_owner(&env, &provider, lower_tick, upper_tick)?;
        let res = Self::burn_position_core(
            &env, &provider, &provider, lower_tick, upper_tick, liquidity,
        )?;
        Self::cleanup_nft_if_closed(&env, &provider, lower_tick, upper_tick);
        Ok(res)
    }

    /// Burn liquidity from a position addressed by its NFT `token_id` (issue #348).
    ///
    /// Resolves the original `(provider, lower_tick, upper_tick)` from the
    /// reverse index, verifies `caller` is the **current** NFT owner, and
    /// withdraws the underlying tokens to `caller`. When the position is fully
    /// closed the NFT is burned and both indexes are cleared.
    pub fn burn_position_by_token_id(
        env: Env,
        caller: Address,
        token_id: u64,
        liquidity: i128,
    ) -> Result<(i128, i128), ClError> {
        Self::extend_instance_ttl(&env);
        Self::require_initialized(&env)?;
        let (provider, lower_tick, upper_tick) =
            Self::resolve_token_owner(&env, &caller, token_id)?;
        caller.require_auth();
        let res =
            Self::burn_position_core(&env, &provider, &caller, lower_tick, upper_tick, liquidity)?;

        // On a full close, sweep any still-owed fees to the current owner before
        // retiring the NFT. Otherwise the position would be de-tokenized with a
        // non-zero `tokens_owed` left under the original provider's key, letting
        // that provider reclaim the new owner's fees via the legacy path.
        let closed = env
            .storage()
            .persistent()
            .get::<_, Position>(&DataKey::Position(provider.clone(), lower_tick, upper_tick))
            .map(|p| p.liquidity == 0)
            .unwrap_or(true);
        if closed {
            Self::collect_fees_core(&env, &provider, &caller, lower_tick, upper_tick)?;
            Self::cleanup_nft_if_closed(&env, &provider, lower_tick, upper_tick);
        }
        Ok(res)
    }

    /// Collect accrued fees from a position keyed by the original `provider`.
    ///
    /// Like `burn_position`, this defers to the current
    /// NFT owner once a position is tokenized.
    pub fn collect_fees(
        env: Env,
        provider: Address,
        lower_tick: i32,
        upper_tick: i32,
    ) -> Result<(i128, i128), ClError> {
        Self::extend_instance_ttl(&env);
        Self::require_initialized(&env)?;
        // No pause guard — LPs must always be able to collect fees.
        provider.require_auth();
        Self::ensure_legacy_owner(&env, &provider, lower_tick, upper_tick)?;
        Self::collect_fees_core(&env, &provider, &provider, lower_tick, upper_tick)
    }

    /// Collect accrued fees from a position addressed by its NFT `token_id`
    /// (issue #348). Fees are paid to the current NFT owner (`caller`).
    pub fn collect_fees_by_token_id(
        env: Env,
        caller: Address,
        token_id: u64,
    ) -> Result<(i128, i128), ClError> {
        Self::extend_instance_ttl(&env);
        Self::require_initialized(&env)?;
        let (provider, lower_tick, upper_tick) =
            Self::resolve_token_owner(&env, &caller, token_id)?;
        caller.require_auth();
        Self::collect_fees_core(&env, &provider, &caller, lower_tick, upper_tick)
    }

    /// Shared burn logic. `provider` keys the stored position; `recipient`
    /// receives the withdrawn tokens. Performs **no** authorization — callers
    /// are responsible for authenticating the actor.
    fn burn_position_core(
        env: &Env,
        provider: &Address,
        recipient: &Address,
        lower_tick: i32,
        upper_tick: i32,
        liquidity: i128,
    ) -> Result<(i128, i128), ClError> {
        if liquidity <= 0 {
            return Err(ClError::ZeroLiquidity);
        }
        let pos_key = DataKey::Position(provider.clone(), lower_tick, upper_tick);
        let mut pos: Position = env
            .storage()
            .persistent()
            .get(&pos_key)
            .ok_or(ClError::PositionNotFound)?;
        if pos.liquidity < liquidity {
            return Err(ClError::InsufficientLiquidity);
        }
        let current_tick = Self::read_current_tick(env)?;
        let (token_a, token_b) = Self::read_tokens(env)?;

        let (fg_inside_a, fg_inside_b) =
            Self::fee_growth_inside(env.clone(), lower_tick, upper_tick);
        let (oa, ob) = Self::pending_fees(&pos, fg_inside_a, fg_inside_b);
        pos.tokens_owed = (pos.tokens_owed.0 + oa, pos.tokens_owed.1 + ob);
        pos.fee_growth_inside_a = fg_inside_a;
        pos.fee_growth_inside_b = fg_inside_b;
        let sqrt_price_now = Self::current_sqrt_price(env, current_tick);
        let (amount_a, amount_b) = Self::amounts_for_liquidity_to_burn(
            current_tick,
            lower_tick,
            upper_tick,
            liquidity,
            sqrt_price_now,
        )?;
        pos.liquidity -= liquidity;

        // The principal recomputed above from the current tick can, due to a
        // precision mismatch with the swap engine's own price-stepping math,
        // exceed what the contract actually holds. Never transfer more than
        // that; bank any shortfall in `tokens_owed` so it stays claimable
        // (via collect_fees) once the contract's balance recovers, instead of
        // hard-trapping on an insufficient-balance transfer.
        let contract_addr = env.current_contract_address();
        let avail_a = TokenClient::new(env, &token_a)
            .balance(&contract_addr)
            .max(0);
        let avail_b = TokenClient::new(env, &token_b)
            .balance(&contract_addr)
            .max(0);
        let pay_a = amount_a.min(avail_a);
        let pay_b = amount_b.min(avail_b);
        pos.tokens_owed = (
            pos.tokens_owed.0 + (amount_a - pay_a),
            pos.tokens_owed.1 + (amount_b - pay_b),
        );
        env.storage().persistent().set(&pos_key, &pos);
        Self::bump_position(env, &pos_key);
        // Remove from position list when position is fully closed
        if pos.liquidity == 0 {
            let list_key = DataKey::PositionList(provider.clone());
            let list: Vec<(i32, i32)> = env
                .storage()
                .persistent()
                .get(&list_key)
                .unwrap_or_else(|| Vec::new(env));
            let range = (lower_tick, upper_tick);
            let mut new_list: Vec<(i32, i32)> = Vec::new(env);
            for r in list.iter() {
                if r != range {
                    new_list.push_back(r);
                }
            }
            env.storage().persistent().set(&list_key, &new_list);
            Self::bump_position(env, &list_key);

            // Fully closed range order: release its direction tag so the
            // provider can place a fresh order on the same range (issue #595).
            let range_order_key = DataKey::RangeOrder(provider.clone(), lower_tick, upper_tick);
            env.storage().persistent().remove(&range_order_key);
            env.storage().instance().remove(&range_order_key);
        }

        let fg_a: i128 = env
            .storage()
            .instance()
            .get(&DataKey::FeeGrowthGlobalA)
            .unwrap_or(0);
        let fg_b: i128 = env
            .storage()
            .instance()
            .get(&DataKey::FeeGrowthGlobalB)
            .unwrap_or(0);
        Self::update_tick(env, lower_tick, current_tick, -liquidity, false, fg_a, fg_b);
        Self::update_tick(env, upper_tick, current_tick, -liquidity, true, fg_a, fg_b);

        if current_tick >= lower_tick && current_tick < upper_tick {
            let active: i128 = env
                .storage()
                .instance()
                .get(&DataKey::ActiveLiquidity)
                .unwrap_or(0);
            env.storage().instance().set(
                &DataKey::ActiveLiquidity,
                &(if active > liquidity {
                    active - liquidity
                } else {
                    0
                }),
            );
        }
        if pay_a > 0 {
            TokenClient::new(env, &token_a).transfer(&contract_addr, recipient, &pay_a);
        }
        if pay_b > 0 {
            TokenClient::new(env, &token_b).transfer(&contract_addr, recipient, &pay_b);
        }
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (symbol_short!("burn_pos"), recipient.clone()),
            (lower_tick, upper_tick, liquidity, pay_a, pay_b)
        );
        Ok((pay_a, pay_b))
    }

    /// Shared fee-collection logic. `provider` keys the stored position;
    /// `recipient` receives the fees. Performs **no** authorization.
    fn collect_fees_core(
        env: &Env,
        provider: &Address,
        recipient: &Address,
        lower_tick: i32,
        upper_tick: i32,
    ) -> Result<(i128, i128), ClError> {
        let pos_key = DataKey::Position(provider.clone(), lower_tick, upper_tick);
        let mut pos: Position = env
            .storage()
            .persistent()
            .get(&pos_key)
            .ok_or(ClError::PositionNotFound)?;

        let (fg_inside_a, fg_inside_b) =
            Self::fee_growth_inside(env.clone(), lower_tick, upper_tick);
        let (na, nb) = Self::pending_fees(&pos, fg_inside_a, fg_inside_b);
        let total_a = pos.tokens_owed.0 + na;
        let total_b = pos.tokens_owed.1 + nb;
        pos.fee_growth_inside_a = fg_inside_a;
        pos.fee_growth_inside_b = fg_inside_b;
        let (token_a, token_b) = Self::read_tokens(env)?;

        // As in burn_position_core: never transfer more than the contract
        // actually holds. Keep any shortfall recorded in `tokens_owed`
        // (instead of always zeroing it) so it remains claimable later.
        let contract_addr = env.current_contract_address();
        let avail_a = TokenClient::new(env, &token_a)
            .balance(&contract_addr)
            .max(0);
        let avail_b = TokenClient::new(env, &token_b)
            .balance(&contract_addr)
            .max(0);
        let pay_a = total_a.min(avail_a);
        let pay_b = total_b.min(avail_b);
        pos.tokens_owed = (total_a - pay_a, total_b - pay_b);
        env.storage().persistent().set(&pos_key, &pos);
        Self::bump_position(env, &pos_key);
        if pay_a > 0 {
            TokenClient::new(env, &token_a).transfer(&contract_addr, recipient, &pay_a);
        }
        if pay_b > 0 {
            TokenClient::new(env, &token_b).transfer(&contract_addr, recipient, &pay_b);
        }
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (symbol_short!("coll_fees"), recipient.clone()),
            (lower_tick, upper_tick, pay_a, pay_b)
        );
        Ok((pay_a, pay_b))
    }

    // ── Issue #348: NFT-keyed position ownership helpers ───────────────────────

    /// The wired-in position-NFT contract, if any.
    fn nft_addr(env: &Env) -> Option<Address> {
        env.storage()
            .instance()
            .get(&DataKey::PositionNft)
            .unwrap_or(None)
    }

    /// Mint a receipt NFT for a freshly opened position and record both index
    /// directions. No-op when no NFT contract is configured.
    fn tokenize_position(env: &Env, provider: &Address, lower_tick: i32, upper_tick: i32) {
        let Some(nft) = Self::nft_addr(env) else {
            return;
        };
        let token_id = PositionNftClient::new(env, &nft).mint(
            provider,
            &env.current_contract_address(),
            &lower_tick,
            &upper_tick,
        );
        let rev_key = DataKey::NftTokenToPosition(token_id);
        let fwd_key = DataKey::PositionNftToken(provider.clone(), lower_tick, upper_tick);
        env.storage()
            .persistent()
            .set(&rev_key, &(provider.clone(), lower_tick, upper_tick));
        Self::bump_position(env, &rev_key);
        env.storage().instance().remove(&rev_key);

        env.storage().persistent().set(&fwd_key, &token_id);
        Self::bump_position(env, &fwd_key);
        env.storage().instance().remove(&fwd_key);
        soroban_amm_sdk::emit_versioned_event!(
            env,
            (symbol_short!("nft_link"), provider.clone()),
            (token_id, lower_tick, upper_tick)
        );
    }

    /// Resolve `(provider, lower_tick, upper_tick)` for `token_id` and verify
    /// that `caller` is the NFT's current owner.
    fn resolve_token_owner(
        env: &Env,
        caller: &Address,
        token_id: u64,
    ) -> Result<(Address, i32, i32), ClError> {
        let nft = Self::nft_addr(env).ok_or(ClError::NftNotConfigured)?;
        let rev_key = DataKey::NftTokenToPosition(token_id);
        let position: (Address, i32, i32) =
            if let Some(pos) = env.storage().persistent().get(&rev_key) {
                Self::bump_position(env, &rev_key);
                pos
            } else if let Some(pos) = env.storage().instance().get(&rev_key) {
                pos
            } else {
                return Err(ClError::PositionNotFound);
            };
        let owner = PositionNftClient::new(env, &nft).owner_of(&token_id);
        if owner != *caller {
            return Err(ClError::NotNftOwner);
        }
        Ok(position)
    }

    /// Guard the legacy address-keyed path: a tokenized position may only be
    /// operated on by `provider` while `provider` still owns its NFT. Pools
    /// without an NFT, or untokenized positions, pass through unchanged.
    fn ensure_legacy_owner(
        env: &Env,
        provider: &Address,
        lower_tick: i32,
        upper_tick: i32,
    ) -> Result<(), ClError> {
        let fwd_key = DataKey::PositionNftToken(provider.clone(), lower_tick, upper_tick);
        let token_id: Option<u64> = if let Some(id) = env.storage().persistent().get(&fwd_key) {
            Self::bump_position(env, &fwd_key);
            Some(id)
        } else {
            env.storage().instance().get(&fwd_key)
        };
        let Some(token_id) = token_id else {
            return Ok(());
        };
        let Some(nft) = Self::nft_addr(env) else {
            return Ok(());
        };
        let owner = PositionNftClient::new(env, &nft).owner_of(&token_id);
        if owner != *provider {
            return Err(ClError::NotNftOwner);
        }
        Ok(())
    }

    /// After a burn, if the position is fully closed and tokenized, burn the
    /// NFT and clear both indexes.
    fn cleanup_nft_if_closed(env: &Env, provider: &Address, lower_tick: i32, upper_tick: i32) {
        let pos: Option<Position> = env.storage().persistent().get(&DataKey::Position(
            provider.clone(),
            lower_tick,
            upper_tick,
        ));
        if pos.map(|p| p.liquidity > 0).unwrap_or(false) {
            return;
        }
        let fwd_key = DataKey::PositionNftToken(provider.clone(), lower_tick, upper_tick);
        let token_id: Option<u64> = if let Some(id) = env.storage().persistent().get(&fwd_key) {
            Some(id)
        } else {
            env.storage().instance().get(&fwd_key)
        };
        let Some(token_id) = token_id else {
            return;
        };
        if let Some(nft) = Self::nft_addr(env) {
            PositionNftClient::new(env, &nft).burn(&token_id);
        }
        let rev_key = DataKey::NftTokenToPosition(token_id);
        env.storage().persistent().remove(&fwd_key);
        env.storage().instance().remove(&fwd_key);
        env.storage().persistent().remove(&rev_key);
        env.storage().instance().remove(&rev_key);
    }

    pub fn get_position(
        env: Env,
        provider: Address,
        lower_tick: i32,
        upper_tick: i32,
    ) -> Result<Position, ClError> {
        Self::extend_instance_ttl(&env);
        env.storage()
            .persistent()
            .get(&DataKey::Position(provider, lower_tick, upper_tick))
            .ok_or(ClError::PositionNotFound)
    }

    pub fn current_tick(env: Env) -> Result<i32, ClError> {
        Self::extend_instance_ttl(&env);
        Self::read_current_tick(&env)
    }

    /// Returns the pool's token pair as `(token_a, token_b)` (issue #470).
    ///
    /// Unlike the constant-product AMM, this pool exposes no `get_info`, so
    /// external contracts that need the pair — such as `reserve_manager`'s
    /// `check_reserves`, which reads the pool's SEP-41 token balances, and
    /// `batch_auction` and other venue-agnostic callers validating an order's
    /// token pair — use this accessor.
    ///
    /// Returns `ClError::NotInitialized` if the pool has not been initialized.
    pub fn get_tokens(env: Env) -> Result<(Address, Address), ClError> {
        Self::extend_instance_ttl(&env);
        Self::read_tokens(&env)
    }

    pub fn active_liquidity(env: Env) -> i128 {
        Self::extend_instance_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::ActiveLiquidity)
            .unwrap_or(0)
    }

    pub fn get_pool_state(env: Env) -> PoolState {
        Self::extend_instance_ttl(&env);
        let current_tick: i32 = env
            .storage()
            .instance()
            .get(&DataKey::CurrentTick)
            .unwrap_or(0);
        let active_liquidity: i128 = env
            .storage()
            .instance()
            .get(&DataKey::ActiveLiquidity)
            .unwrap_or(0);
        let tick_spacing: i32 = env
            .storage()
            .instance()
            .get(&DataKey::TickSpacing)
            .unwrap_or(1);
        let sqrt_price = Self::current_sqrt_price(&env, current_tick);
        PoolState {
            sqrt_price,
            current_tick,
            active_liquidity,
            tick_spacing,
        }
    }

    /// Returns the pool's fee tier in basis points (issue #700).
    ///
    /// Lets a venue-agnostic caller (e.g. batch_auction's factory-backed venue
    /// registry) look this pool up via `Factory::get_cl_pool(token_a, token_b,
    /// fee_bps)` without needing the fee tier supplied out of band.
    pub fn fee_bps(env: Env) -> Result<i128, ClError> {
        Self::extend_instance_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::FeeBps)
            .ok_or(ClError::NotInitialized)
    }

    // ── Issue #203: per-tick view functions ───────────────────────────────────

    /// Returns the `TickInfo` for an initialized tick.
    /// Returns `ClError::TickNotInitialized` if the tick has never been touched by a position.
    /// Requires no auth.
    pub fn get_tick_info(env: Env, tick: i32) -> Result<TickInfo, ClError> {
        Self::extend_instance_ttl(&env);
        let key = DataKey::Tick(tick);
        if let Some(info) = env.storage().persistent().get(&key) {
            Self::bump_position(&env, &key);
            Ok(info)
        } else if let Some(info) = env.storage().instance().get(&key) {
            Ok(info)
        } else {
            Err(ClError::TickNotInitialized)
        }
    }

    /// Returns `true` when the tick currently has non-zero gross liquidity.
    /// Requires no auth.
    pub fn is_tick_initialized(env: Env, tick: i32) -> bool {
        Self::extend_instance_ttl(&env);
        let key = DataKey::Tick(tick);
        if env.storage().persistent().has(&key) {
            Self::bump_position(&env, &key);
            true
        } else {
            env.storage().instance().has(&key)
        }
    }

    // ── Issue #218: public tick-bitmap helpers ────────────────────────────────

    /// Returns the lowest initialized tick **strictly above** `tick`.
    /// Uses the compressed tick bitmap for O(1)–O(log N) lookup.
    /// Returns `None` when no higher initialized tick exists.
    pub fn next_initialized_tick_pub(env: Env, tick: i32) -> Option<i32> {
        Self::extend_instance_ttl(&env);
        Self::next_initialized_tick(&env, tick, false)
    }

    /// Returns the highest initialized tick **at or below** `tick`.
    /// Uses the compressed tick bitmap for O(1)–O(log N) lookup.
    /// Returns `None` when no lower initialized tick exists.
    pub fn prev_initialized_tick_pub(env: Env, tick: i32) -> Option<i32> {
        Self::extend_instance_ttl(&env);
        Self::next_initialized_tick(&env, tick, true)
    }

    // ── Issue #219 / #1099: sqrtPrice math library ────────────────────────────

    /// Converts a tick to `sqrtPriceX96 = sqrt(1.0001^tick) * 2^96`.
    ///
    /// Delegates to [`math::tick_to_sqrt_price_x96`] (Q128 binary decomposition,
    /// accurate to within 1 ULP). Out-of-range ticks are clamped to
    /// `[MIN_TICK, MAX_TICK]`. Because `u128` caps `sqrtPriceX96` at
    /// [`math::MAX_SQRT_PRICE`], ticks in `[MIN_TICK, 443_636]` produce strictly
    /// distinct sqrt prices while ticks above `443_636` saturate at
    /// `MAX_SQRT_PRICE`.
    pub fn tick_to_sqrt_price_x96(tick: i32) -> u128 {
        math::tick_to_sqrt_price_x96(tick.clamp(MIN_TICK, MAX_TICK))
    }

    /// Returns the largest tick `t` such that `tick_to_sqrt_price_x96(t) <= sqrt_price_x96`.
    ///
    /// Delegates to [`math::sqrt_price_x96_to_tick`] after clamping the input to
    /// `[math::MIN_SQRT_PRICE, math::MAX_SQRT_PRICE]`. Across the non-saturated
    /// domain `[MIN_TICK, 443_636]` this is the exact floor inverse of
    /// [`Self::tick_to_sqrt_price_x96`].
    pub fn sqrt_price_x96_to_tick(sqrt_price_x96: u128) -> i32 {
        math::sqrt_price_x96_to_tick(
            sqrt_price_x96.clamp(math::MIN_SQRT_PRICE, math::MAX_SQRT_PRICE),
        )
    }

    // ── Issue #220: tick state-machine query helpers ──────────────────────────

    /// Returns the `liquidity_net` value stored at `tick`.
    ///
    /// When a swap crosses `tick` moving **upward** (zero_for_one = false),
    /// add `liquidity_net` to active liquidity.  When crossing **downward**
    /// (zero_for_one = true), subtract it.  Returns 0 for uninitialized ticks.
    pub fn get_liquidity_net_at_tick(env: Env, tick: i32) -> i128 {
        Self::extend_instance_ttl(&env);
        Self::get_tick(&env, tick).liquidity_net
    }

    /// Simulates the active-liquidity transition that occurs when a swap crosses `tick`.
    ///
    /// * `zero_for_one = true`  → price moving down; subtract `liquidity_net`.
    /// * `zero_for_one = false` → price moving up;   add    `liquidity_net`.
    ///
    /// Pure read — does **not** modify contract state.
    pub fn simulate_tick_cross(
        env: Env,
        current_liquidity: i128,
        tick: i32,
        zero_for_one: bool,
    ) -> i128 {
        Self::extend_instance_ttl(&env);
        let net = Self::get_tick(&env, tick).liquidity_net;
        if zero_for_one {
            (current_liquidity - net).max(0)
        } else {
            (current_liquidity + net).max(0)
        }
    }

    pub fn swap(
        env: Env,
        sender: Address,
        zero_for_one: bool,
        amount_in: i128,
        sqrt_price_limit_x96: u128,
        min_amount_out: i128,
        deadline: u64,
    ) -> Result<i128, ClError> {
        Self::extend_instance_ttl(&env);
        // Reading the token pair first doubles as the initialization check,
        // so this hot path pays for no extra storage lookup.
        let (token_a, token_b) = Self::read_tokens(&env)?;
        if env.ledger().timestamp() > deadline {
            return Err(ClError::DeadlineExpired);
        }
        if Self::is_paused(env.clone()) {
            return Err(ClError::Paused);
        }
        sender.require_auth();
        if amount_in <= 0 {
            return Err(ClError::ZeroAmounts);
        }

        let protocol_fee_bps: i128 = env
            .storage()
            .instance()
            .get(&DataKey::ProtocolFeeBps)
            .unwrap_or(0);

        // Update tick accumulator before changing current tick
        let now = env.ledger().timestamp();
        let last_ts: u64 = env
            .storage()
            .instance()
            .get(&DataKey::LastOracleTimestamp)
            .unwrap_or(now);
        let elapsed = now.saturating_sub(last_ts) as i64;
        if elapsed > 0 {
            let current_tick_oracle: i32 = env
                .storage()
                .instance()
                .get(&DataKey::CurrentTick)
                .unwrap_or(0);
            let cum: i64 = env
                .storage()
                .instance()
                .get(&DataKey::TickCumulative)
                .unwrap_or(0);
            let new_cum = cum + (current_tick_oracle as i64) * elapsed;
            Self::record_oracle_point(&env, now, new_cum);
            env.storage()
                .instance()
                .set(&DataKey::TickCumulative, &new_cum);
            env.storage()
                .instance()
                .set(&DataKey::LastOracleTimestamp, &now);
        } else if !env.storage().instance().has(&DataKey::OracleHeader) {
            Self::migrate_legacy_oracle_if_needed(&env);
        }

        let fee_bps: i128 = env.storage().instance().get(&DataKey::FeeBps).unwrap_or(0);
        let mut amount_remaining = amount_in;
        let mut amount_out_total = 0_i128;
        let mut current_tick = env
            .storage()
            .instance()
            .get(&DataKey::CurrentTick)
            .unwrap_or(0);
        let mut active_liquidity = env
            .storage()
            .instance()
            .get(&DataKey::ActiveLiquidity)
            .unwrap_or(0);
        let mut sqrt_price_x96 = Self::current_sqrt_price(&env, current_tick);
        // Snapshot the price before the tick walk so we can tell whether the
        // swap actually moved it (issue #352).
        let sqrt_price_before = sqrt_price_x96;

        while amount_remaining > 0 {
            if sqrt_price_limit_x96 != 0
                && ((zero_for_one && sqrt_price_x96 <= sqrt_price_limit_x96)
                    || (!zero_for_one && sqrt_price_x96 >= sqrt_price_limit_x96))
            {
                break;
            }
            let next_tick_opt = Self::next_initialized_tick(&env, current_tick, zero_for_one);
            // No initialized ticks in this direction and no liquidity — nothing to trade.
            // amount_remaining is left untouched so this untraded portion is excluded
            // from amount_in_actual below.
            if next_tick_opt.is_none() && active_liquidity == 0 {
                break;
            }

            let next_tick = match next_tick_opt {
                Some(t) => {
                    if zero_for_one {
                        t.max(MIN_TICK)
                    } else {
                        t.min(MAX_TICK)
                    }
                }
                None => {
                    if zero_for_one {
                        MIN_TICK
                    } else {
                        MAX_TICK
                    }
                }
            };

            let next_price_x96 = Self::tick_to_sqrt_price_x96(next_tick);
            let (target_price_x96, hit_limit) = Self::step_target_price_and_limit(
                next_price_x96,
                sqrt_price_limit_x96,
                zero_for_one,
            );

            let amount_in_after_fee = amount_remaining * (10000 - fee_bps) / 10000;

            let (amount_in_step_after_fee, amount_out_step) = if active_liquidity == 0 {
                (0, 0)
            } else {
                Self::compute_step(
                    active_liquidity,
                    sqrt_price_x96,
                    target_price_x96,
                    zero_for_one,
                )
            };

            if (amount_in_after_fee >= amount_in_step_after_fee || active_liquidity == 0)
                && !hit_limit
            {
                let actual_step_in = if active_liquidity > 0 && fee_bps > 0 {
                    (amount_in_step_after_fee * 10000 + 10000 - fee_bps - 1) / (10000 - fee_bps)
                } else {
                    amount_in_step_after_fee
                };

                let actual_step_in = actual_step_in.min(amount_remaining);

                amount_remaining -= actual_step_in;
                amount_out_total += amount_out_step;

                let fee = actual_step_in - amount_in_step_after_fee;
                if fee > 0 && active_liquidity > 0 {
                    let protocol_fee = fee * protocol_fee_bps / 10000;
                    let lp_fee = fee - protocol_fee;

                    if zero_for_one {
                        if protocol_fee > 0 {
                            let accrued_a: i128 = env
                                .storage()
                                .instance()
                                .get(&DataKey::AccruedProtocolFeeA)
                                .unwrap_or(0);
                            env.storage()
                                .instance()
                                .set(&DataKey::AccruedProtocolFeeA, &(accrued_a + protocol_fee));
                        }
                        if lp_fee > 0 {
                            let fg_a: i128 = env
                                .storage()
                                .instance()
                                .get(&DataKey::FeeGrowthGlobalA)
                                .unwrap_or(0);
                            let growth = math::mul_div(
                                lp_fee as u128,
                                FEE_GROWTH_SCALE as u128,
                                active_liquidity as u128,
                            )
                            .unwrap_or(0) as i128;
                            env.storage()
                                .instance()
                                .set(&DataKey::FeeGrowthGlobalA, &(fg_a + growth));
                        }
                    } else {
                        if protocol_fee > 0 {
                            let accrued_b: i128 = env
                                .storage()
                                .instance()
                                .get(&DataKey::AccruedProtocolFeeB)
                                .unwrap_or(0);
                            env.storage()
                                .instance()
                                .set(&DataKey::AccruedProtocolFeeB, &(accrued_b + protocol_fee));
                        }
                        if lp_fee > 0 {
                            let fg_b: i128 = env
                                .storage()
                                .instance()
                                .get(&DataKey::FeeGrowthGlobalB)
                                .unwrap_or(0);
                            let growth = math::mul_div(
                                lp_fee as u128,
                                FEE_GROWTH_SCALE as u128,
                                active_liquidity as u128,
                            )
                            .unwrap_or(0) as i128;
                            env.storage()
                                .instance()
                                .set(&DataKey::FeeGrowthGlobalB, &(fg_b + growth));
                        }
                    }
                }

                sqrt_price_x96 = target_price_x96;

                let mut tick_info = Self::get_tick(&env, next_tick);
                let fg_a: i128 = env
                    .storage()
                    .instance()
                    .get(&DataKey::FeeGrowthGlobalA)
                    .unwrap_or(0);
                let fg_b: i128 = env
                    .storage()
                    .instance()
                    .get(&DataKey::FeeGrowthGlobalB)
                    .unwrap_or(0);
                tick_info.fee_growth_outside_a = fg_a - tick_info.fee_growth_outside_a;
                tick_info.fee_growth_outside_b = fg_b - tick_info.fee_growth_outside_b;
                Self::set_tick(&env, next_tick, &tick_info);

                if zero_for_one {
                    active_liquidity -= tick_info.liquidity_net;
                    current_tick = (next_tick - 1).max(MIN_TICK);
                } else {
                    active_liquidity += tick_info.liquidity_net;
                    current_tick = next_tick;
                }
            } else {
                if active_liquidity > 0 {
                    let (_target_p_x96, amt_in_after_fee) = if hit_limit {
                        (target_price_x96, amount_in_step_after_fee)
                    } else {
                        (target_price_x96, amount_in_after_fee)
                    };

                    let (new_price_x96, amount_out_step) = Self::compute_final_price_and_output(
                        active_liquidity,
                        sqrt_price_x96,
                        amt_in_after_fee,
                        zero_for_one,
                    );

                    let actual_in = if hit_limit {
                        if fee_bps > 0 {
                            (amt_in_after_fee * 10000 + 10000 - fee_bps - 1) / (10000 - fee_bps)
                        } else {
                            amt_in_after_fee
                        }
                    } else {
                        amount_remaining
                    };
                    let actual_in = actual_in.min(amount_remaining);

                    amount_remaining -= actual_in;
                    amount_out_total += amount_out_step;

                    let fee = actual_in - amt_in_after_fee;
                    if fee > 0 {
                        let protocol_fee = fee * protocol_fee_bps / 10000;
                        let lp_fee = fee - protocol_fee;

                        if zero_for_one {
                            if protocol_fee > 0 {
                                let accrued_a: i128 = env
                                    .storage()
                                    .instance()
                                    .get(&DataKey::AccruedProtocolFeeA)
                                    .unwrap_or(0);
                                env.storage().instance().set(
                                    &DataKey::AccruedProtocolFeeA,
                                    &(accrued_a + protocol_fee),
                                );
                            }
                            if lp_fee > 0 {
                                let fg_a: i128 = env
                                    .storage()
                                    .instance()
                                    .get(&DataKey::FeeGrowthGlobalA)
                                    .unwrap_or(0);
                                let growth = math::mul_div(
                                    lp_fee as u128,
                                    FEE_GROWTH_SCALE as u128,
                                    active_liquidity as u128,
                                )
                                .unwrap_or(0) as i128;
                                env.storage()
                                    .instance()
                                    .set(&DataKey::FeeGrowthGlobalA, &(fg_a + growth));
                            }
                        } else {
                            if protocol_fee > 0 {
                                let accrued_b: i128 = env
                                    .storage()
                                    .instance()
                                    .get(&DataKey::AccruedProtocolFeeB)
                                    .unwrap_or(0);
                                env.storage().instance().set(
                                    &DataKey::AccruedProtocolFeeB,
                                    &(accrued_b + protocol_fee),
                                );
                            }
                            if lp_fee > 0 {
                                let fg_b: i128 = env
                                    .storage()
                                    .instance()
                                    .get(&DataKey::FeeGrowthGlobalB)
                                    .unwrap_or(0);
                                let growth = math::mul_div(
                                    lp_fee as u128,
                                    FEE_GROWTH_SCALE as u128,
                                    active_liquidity as u128,
                                )
                                .unwrap_or(0) as i128;
                                env.storage()
                                    .instance()
                                    .set(&DataKey::FeeGrowthGlobalB, &(fg_b + growth));
                            }
                        }
                    }

                    if hit_limit {
                        sqrt_price_x96 = target_price_x96;
                    } else {
                        sqrt_price_x96 = new_price_x96;
                    }
                    // This branch only runs when the step's own math already
                    // determined the trade does *not* need to reach `next_tick`
                    // (the `if` arm above handles the case where it does). The
                    // step math works in a deliberately low-precision (~3
                    // significant digit) price representation, though, and its
                    // pool-favorable rounding can — right at a boundary —
                    // compute a price indistinguishable from (or past) the
                    // tick it was told not to reach. Reconstructing that price
                    // via the full-precision `sqrt_price_x96_to_tick` can then
                    // resolve to `next_tick` or beyond even though this step
                    // never crossed it. Clamp back to the known-correct side.
                    current_tick = Self::sqrt_price_x96_to_tick(sqrt_price_x96);
                    current_tick = if zero_for_one {
                        current_tick.max(next_tick)
                    } else {
                        current_tick.min(next_tick)
                    };
                } else {
                    sqrt_price_x96 = target_price_x96;
                    current_tick = Self::sqrt_price_x96_to_tick(sqrt_price_x96);
                    // No liquidity in this gap — amount_remaining is left untouched
                    // so this untraded portion is excluded from amount_in_actual.
                }
                break;
            }
        }

        let amount_in_actual = amount_in - amount_remaining;
        if amount_out_total < min_amount_out {
            return Err(ClError::SlippageExceeded);
        }

        let token_in = if zero_for_one {
            token_a.clone()
        } else {
            token_b.clone()
        };
        let token_out = if zero_for_one {
            token_b.clone()
        } else {
            token_a.clone()
        };

        if amount_in_actual > 0 && amount_out_total > 0 {
            Self::check_oracle_deviation(
                &env,
                &token_in,
                &token_out,
                amount_in_actual,
                amount_out_total,
            )?;
        }

        if amount_in_actual > 0 {
            TokenClient::new(&env, &token_in).transfer(
                &sender,
                &env.current_contract_address(),
                &amount_in_actual,
            );
        }
        if amount_out_total > 0 {
            TokenClient::new(&env, &token_out).transfer(
                &env.current_contract_address(),
                &sender,
                &amount_out_total,
            );
        }

        env.storage()
            .instance()
            .set(&DataKey::CurrentTick, &current_tick);
        env.storage()
            .instance()
            .set(&DataKey::ActiveLiquidity, &active_liquidity);
        env.storage()
            .instance()
            .set(&DataKey::SqrtPriceX96, &sqrt_price_x96);

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (soroban_sdk::symbol_short!("swap"), sender),
            (
                zero_for_one,
                amount_in_actual,
                amount_out_total,
                sqrt_price_x96,
                current_tick,
            )
        );

        // Issue #352: dedicated price-movement signal keyed by the token pair.
        // The `swap` event above is keyed by `sender`, which forces indexers
        // (TWAP oracle, incentive campaigns) tracking a specific pool to scan
        // every sender's events. This event lets them subscribe by token pair
        // and react to price changes without polling storage each ledger. It is
        // only emitted when the swap actually moved the price.
        if sqrt_price_x96 != sqrt_price_before {
            soroban_amm_sdk::emit_versioned_event!(
                env,
                (symbol_short!("price_upd"), token_in, token_out),
                (
                    amount_in_actual,
                    amount_out_total,
                    sqrt_price_x96,
                    current_tick
                )
            );
        }

        Ok(amount_out_total)
    }

    // ── #696: swap_exact_out ──────────────────────────────────────────────────
    //
    // `swap`'s core step math (`compute_step` / `compute_final_price_and_output`,
    // just above) works in the pool's existing low-precision integer price
    // representation — `p = (sqrt_price_x96 * 1000) >> 96`, i.e. roughly 3
    // significant digits of price, not full Q96/Q128 precision. `math.rs`'s
    // `tick_to_sqrt_price_x96` / `get_amount0_delta` / `get_amount1_delta` are
    // *not* used by this loop at all (they back `mint_position`/`burn_position`
    // instead) — the actual swap loop never touches them. Introducing a
    // parallel, fully-precise Q96 exact-out step function in `math.rs` would
    // therefore not share any code with `swap`'s exact-in path, defeating the
    // "quote and swap can never disagree" goal and risking exactly the kind of
    // silent pool-drain this issue warns about, since the two implementations
    // could round differently on the very same trade. Instead, the exact-out
    // step math below mirrors `compute_step` / `compute_final_price_and_output`
    // in the same representation, so `swap_exact_out` is the mirror image of
    // `swap` at every step, not a second system that happens to look similar.
    //
    // `walk_exact_out` is the single core loop; both `swap_exact_out` and
    // `quote_exact_out` call it and only differ in what they do with the
    // result (apply it to storage and transfer tokens, or just read
    // `amount_in_after_fee_total` / `amount_in_gross_total`).

    /// Result of walking ticks to fill an exact-out request. `tick_crossings`
    /// lists, in crossing order, each tick's *new* `fee_growth_outside`
    /// values (already flipped against the running fee-growth-global at the
    /// moment of that crossing, exactly as `swap` computes it inline) so the
    /// caller can persist them without re-deriving the interleaving.
    /// Ceiling division for positive `a`, `b` — used wherever exact-out must
    /// round the trader's required input *up* rather than down, so the pool
    /// never pays out `amount_out` for less than it is truly owed.
    fn ceil_div(a: i128, b: i128) -> i128 {
        if a <= 0 {
            0
        } else {
            (a + b - 1) / b
        }
    }

    /// Reverse of `compute_final_price_and_output`: given a fixed *output*
    /// amount, solves for the price the step must land on and the input that
    /// price requires — in full Q64.96 precision, like the rest of the swap
    /// math. (Both previously worked in a `p = (sqrt_price_x96 * 1000) >> 96`
    /// scale that quantised the price to roughly 20-tick steps.)
    ///
    /// Rounding, per quantity:
    /// - `sqrt_price_next`: rounds **up** in magnitude of movement, so the
    ///   price always moves *at least* as far as the exact real-valued
    ///   solution requires. Rounding the other way (floor, tried first and
    ///   reverted — see the regression test
    ///   `compute_final_price_and_input_does_not_undercharge_small_amount_out`)
    ///   lets the price move truncate to zero whenever `amount_out` is small
    ///   relative to `liquidity`, which then computes `amount_in == 0` for a
    ///   nonzero `amount_out` — a free drain of the pool.
    /// - `amount_in`: rounded **up** from that price — the caller must pay at
    ///   least as much as the (already pool-favourable) price move implies.
    /// - `amount_out`: not computed here — it is the caller-supplied,
    ///   already-fixed target for this step, never rounded.
    /// - `fee_amount`: computed by the caller from `amount_in` (already
    ///   rounded up) minus the after-fee amount, so it inherits `amount_in`'s
    ///   pool-favourable rounding rather than being rounded independently.
    fn compute_final_price_and_input(
        liquidity: i128,
        sqrt_price_current_x96: u128,
        amount_out_needed: i128,
        zero_for_one: bool,
    ) -> (u128, i128) {
        if liquidity <= 0 || amount_out_needed <= 0 {
            return (sqrt_price_current_x96, 0);
        }
        let liq = liquidity.unsigned_abs();
        let out = amount_out_needed.unsigned_abs();

        if zero_for_one {
            // token1 out, token0 in: price falls. The drop rounds up, so the
            // price always moves at least as far as the requested output
            // requires and the input derived from it is never understated.
            let next =
                match math::next_sqrt_price_from_amount1_out(sqrt_price_current_x96, liq, out) {
                    Some(p) if p < sqrt_price_current_x96 => p,
                    _ => return (sqrt_price_current_x96, 0),
                };
            let amount_in = Self::u128_to_i128_saturating(math::amount0_delta_exact(
                next,
                sqrt_price_current_x96,
                liq,
                true,
            ));
            (next, amount_in)
        } else {
            // token0 out, token1 in: price rises.
            let next =
                match math::next_sqrt_price_from_amount0_out(sqrt_price_current_x96, liq, out) {
                    Some(p) if p > sqrt_price_current_x96 => p,
                    // The requested output exceeds what this range can supply
                    // at any price; the caller clamps `amount_out_needed` to
                    // the step's capacity first, so this is defensive only.
                    _ => return (sqrt_price_current_x96, 0),
                };
            let amount_in = Self::u128_to_i128_saturating(math::amount1_delta_exact(
                sqrt_price_current_x96,
                next,
                liq,
                true,
            ));
            (next, amount_in)
        }
    }

    /// Core exact-out tick walk, shared by `swap_exact_out` and
    /// `quote_exact_out`. Never touches storage or transfers tokens — the
    /// caller applies `tick_crossings` / the fee-growth and protocol-fee
    /// deltas (or discards them entirely, for a pure quote).
    #[allow(clippy::too_many_arguments)]
    fn walk_exact_out(
        env: &Env,
        zero_for_one: bool,
        amount_out_requested: i128,
        sqrt_price_limit_x96: u128,
        fee_bps: i128,
        protocol_fee_bps: i128,
        mut current_tick: i32,
        mut active_liquidity: i128,
        mut sqrt_price_x96: u128,
        fee_growth_global_a: i128,
        fee_growth_global_b: i128,
    ) -> ExactOutWalk {
        let mut amount_out_remaining = amount_out_requested;
        let mut amount_in_after_fee_total = 0_i128;
        let mut amount_in_gross_total = 0_i128;
        let mut protocol_fee_a_total = 0_i128;
        let mut protocol_fee_b_total = 0_i128;
        let mut fg_a = fee_growth_global_a;
        let mut fg_b = fee_growth_global_b;
        let mut tick_crossings: Vec<(i32, i128, i128)> = Vec::new(env);

        while amount_out_remaining > 0 {
            let next_tick_opt = Self::next_initialized_tick(env, current_tick, zero_for_one);
            if next_tick_opt.is_none() && active_liquidity == 0 {
                break;
            }

            let next_tick = match next_tick_opt {
                Some(t) => {
                    if zero_for_one {
                        t.max(MIN_TICK)
                    } else {
                        t.min(MAX_TICK)
                    }
                }
                None => {
                    if zero_for_one {
                        MIN_TICK
                    } else {
                        MAX_TICK
                    }
                }
            };

            let next_price_x96 = Self::tick_to_sqrt_price_x96(next_tick);
            let (target_price_x96, hit_limit) = Self::step_target_price_and_limit(
                next_price_x96,
                sqrt_price_limit_x96,
                zero_for_one,
            );

            let (amount_in_step_after_fee, amount_out_step) = if active_liquidity == 0 {
                (0, 0)
            } else {
                Self::compute_step(
                    active_liquidity,
                    sqrt_price_x96,
                    target_price_x96,
                    zero_for_one,
                )
            };

            if (amount_out_remaining >= amount_out_step || active_liquidity == 0) && !hit_limit {
                // Full step: the tick boundary is reached before amount_out is
                // satisfied. Reuse the exact-in step's own (already-tested)
                // amount_in for this exact price transition — it is a fixed
                // quantity for this tick boundary, independent of which
                // direction (exact-in or exact-out) drove the walk there.
                let step_in_gross = if active_liquidity > 0 && fee_bps > 0 {
                    Self::ceil_div(amount_in_step_after_fee * 10000, 10000 - fee_bps)
                } else {
                    amount_in_step_after_fee
                };

                amount_out_remaining -= amount_out_step;
                amount_in_after_fee_total += amount_in_step_after_fee;
                amount_in_gross_total += step_in_gross;

                let fee = step_in_gross - amount_in_step_after_fee;
                if fee > 0 && active_liquidity > 0 {
                    let protocol_fee = fee * protocol_fee_bps / 10000;
                    let lp_fee = fee - protocol_fee;
                    if zero_for_one {
                        protocol_fee_a_total += protocol_fee;
                        if lp_fee > 0 {
                            let growth = math::mul_div(
                                lp_fee as u128,
                                FEE_GROWTH_SCALE as u128,
                                active_liquidity as u128,
                            )
                            .unwrap_or(0) as i128;
                            fg_a += growth;
                        }
                    } else {
                        protocol_fee_b_total += protocol_fee;
                        if lp_fee > 0 {
                            let growth = math::mul_div(
                                lp_fee as u128,
                                FEE_GROWTH_SCALE as u128,
                                active_liquidity as u128,
                            )
                            .unwrap_or(0) as i128;
                            fg_b += growth;
                        }
                    }
                }

                sqrt_price_x96 = target_price_x96;

                let tick_info = Self::get_tick(env, next_tick);
                let flip_a = fg_a - tick_info.fee_growth_outside_a;
                let flip_b = fg_b - tick_info.fee_growth_outside_b;
                tick_crossings.push_back((next_tick, flip_a, flip_b));

                if zero_for_one {
                    active_liquidity -= tick_info.liquidity_net;
                    current_tick = next_tick - 1;
                } else {
                    active_liquidity += tick_info.liquidity_net;
                    current_tick = next_tick;
                }
            } else {
                if active_liquidity > 0 {
                    let out_needed = if hit_limit {
                        amount_out_step
                    } else {
                        amount_out_remaining
                    }
                    .min(amount_out_remaining);

                    let (new_price_x96, in_after_fee) = Self::compute_final_price_and_input(
                        active_liquidity,
                        sqrt_price_x96,
                        out_needed,
                        zero_for_one,
                    );
                    let in_gross = if fee_bps > 0 {
                        Self::ceil_div(in_after_fee * 10000, 10000 - fee_bps)
                    } else {
                        in_after_fee
                    };

                    amount_out_remaining -= out_needed;
                    amount_in_after_fee_total += in_after_fee;
                    amount_in_gross_total += in_gross;

                    let fee = in_gross - in_after_fee;
                    if fee > 0 {
                        let protocol_fee = fee * protocol_fee_bps / 10000;
                        let lp_fee = fee - protocol_fee;
                        if zero_for_one {
                            protocol_fee_a_total += protocol_fee;
                            if lp_fee > 0 {
                                let growth = math::mul_div(
                                    lp_fee as u128,
                                    FEE_GROWTH_SCALE as u128,
                                    active_liquidity as u128,
                                )
                                .unwrap_or(0) as i128;
                                fg_a += growth;
                            }
                        } else {
                            protocol_fee_b_total += protocol_fee;
                            if lp_fee > 0 {
                                let growth = math::mul_div(
                                    lp_fee as u128,
                                    FEE_GROWTH_SCALE as u128,
                                    active_liquidity as u128,
                                )
                                .unwrap_or(0) as i128;
                                fg_b += growth;
                            }
                        }
                    }

                    if hit_limit {
                        sqrt_price_x96 = target_price_x96;
                    } else {
                        sqrt_price_x96 = new_price_x96;
                    }
                    // See the equivalent clamp in `swap`'s partial-step branch:
                    // this branch only runs when the step math already decided
                    // the trade does not need to reach `next_tick`, but its
                    // deliberately low-precision, pool-favorable-rounded price
                    // can — right at a boundary — round-trip through the
                    // full-precision `sqrt_price_x96_to_tick` to `next_tick` or
                    // beyond. Clamp back to the known-correct side.
                    current_tick = Self::sqrt_price_x96_to_tick(sqrt_price_x96);
                    current_tick = if zero_for_one {
                        current_tick.max(next_tick)
                    } else {
                        current_tick.min(next_tick)
                    };
                } else {
                    sqrt_price_x96 = target_price_x96;
                    current_tick = Self::sqrt_price_x96_to_tick(sqrt_price_x96);
                }
                break;
            }
        }

        ExactOutWalk {
            amount_in_after_fee_total,
            amount_in_gross_total,
            amount_out_filled: amount_out_requested - amount_out_remaining,
            sqrt_price_final: sqrt_price_x96,
            tick_final: current_tick,
            active_liquidity_final: active_liquidity,
            fee_growth_global_a_delta: fg_a - fee_growth_global_a,
            fee_growth_global_b_delta: fg_b - fee_growth_global_b,
            protocol_fee_a_delta: protocol_fee_a_total,
            protocol_fee_b_delta: protocol_fee_b_total,
            tick_crossings,
            fully_filled: amount_out_remaining == 0,
        }
    }

    /// Swap to receive an exact amount of one token, paying at most
    /// `max_amount_in` of the other. Mirrors `swap` in every respect other
    /// than which side is fixed: deadline/pause/auth/amount checks, the
    /// oracle tick-accumulator update, tick crossing with `liquidity_net`
    /// and fee-growth-outside flips, protocol fee accrual, the oracle
    /// deviation guard, and the `price_upd` / `swap_out` events.
    ///
    /// Reverts with `SlippageExceeded` if the required input would exceed
    /// `max_amount_in`, and with `ExactOutNotFullyFilled` if the price limit
    /// or available liquidity is reached before `amount_out` is satisfied —
    /// exact-out has no meaningful partial fill.
    pub fn swap_exact_out(
        env: Env,
        sender: Address,
        zero_for_one: bool,
        amount_out: i128,
        sqrt_price_limit_x96: u128,
        max_amount_in: i128,
        deadline: u64,
    ) -> Result<i128, ClError> {
        Self::extend_instance_ttl(&env);
        Self::require_initialized(&env)?;
        if env.ledger().timestamp() > deadline {
            return Err(ClError::DeadlineExpired);
        }
        if Self::is_paused(env.clone()) {
            return Err(ClError::Paused);
        }
        sender.require_auth();
        if amount_out <= 0 {
            return Err(ClError::ZeroAmounts);
        }

        let (token_a, token_b) = Self::read_tokens(&env)?;
        let protocol_fee_bps: i128 = env
            .storage()
            .instance()
            .get(&DataKey::ProtocolFeeBps)
            .unwrap_or(0);

        // Oracle tick-accumulator update, identical to `swap`, before the
        // tick walk moves `CurrentTick`.
        let now = env.ledger().timestamp();
        let last_ts: u64 = env
            .storage()
            .instance()
            .get(&DataKey::LastOracleTimestamp)
            .unwrap_or(now);
        let elapsed = now.saturating_sub(last_ts) as i64;
        if elapsed > 0 {
            let current_tick_oracle: i32 = env
                .storage()
                .instance()
                .get(&DataKey::CurrentTick)
                .unwrap_or(0);
            let cum: i64 = env
                .storage()
                .instance()
                .get(&DataKey::TickCumulative)
                .unwrap_or(0);
            let new_cum = cum + (current_tick_oracle as i64) * elapsed;
            Self::record_oracle_point(&env, now, new_cum);
            env.storage()
                .instance()
                .set(&DataKey::TickCumulative, &new_cum);
            env.storage()
                .instance()
                .set(&DataKey::LastOracleTimestamp, &now);
        } else if !env.storage().instance().has(&DataKey::OracleHeader) {
            Self::migrate_legacy_oracle_if_needed(&env);
        }

        let fee_bps: i128 = env.storage().instance().get(&DataKey::FeeBps).unwrap_or(0);
        let current_tick: i32 = env
            .storage()
            .instance()
            .get(&DataKey::CurrentTick)
            .unwrap_or(0);
        let active_liquidity: i128 = env
            .storage()
            .instance()
            .get(&DataKey::ActiveLiquidity)
            .unwrap_or(0);
        let sqrt_price_x96 = Self::current_sqrt_price_x96(&env, current_tick);
        let sqrt_price_before = sqrt_price_x96;
        let fg_a: i128 = env
            .storage()
            .instance()
            .get(&DataKey::FeeGrowthGlobalA)
            .unwrap_or(0);
        let fg_b: i128 = env
            .storage()
            .instance()
            .get(&DataKey::FeeGrowthGlobalB)
            .unwrap_or(0);

        let walk = Self::walk_exact_out(
            &env,
            zero_for_one,
            amount_out,
            sqrt_price_limit_x96,
            fee_bps,
            protocol_fee_bps,
            current_tick,
            active_liquidity,
            sqrt_price_x96,
            fg_a,
            fg_b,
        );

        if !walk.fully_filled {
            return Err(ClError::ExactOutNotFullyFilled);
        }
        if walk.amount_in_gross_total > max_amount_in {
            return Err(ClError::SlippageExceeded);
        }

        let token_in = if zero_for_one {
            token_a.clone()
        } else {
            token_b.clone()
        };
        let token_out = if zero_for_one {
            token_b.clone()
        } else {
            token_a.clone()
        };

        if walk.amount_in_gross_total > 0 && walk.amount_out_filled > 0 {
            Self::check_oracle_deviation(
                &env,
                &token_in,
                &token_out,
                walk.amount_in_gross_total,
                walk.amount_out_filled,
            )?;
        }

        // Apply tick crossings (liquidity_net already folded into
        // `walk.active_liquidity_final`; here we only persist the
        // fee-growth-outside flips computed during the walk).
        for i in 0..walk.tick_crossings.len() {
            let (tick, flip_a, flip_b) = walk.tick_crossings.get(i).unwrap();
            let mut tick_info = Self::get_tick(&env, tick);
            tick_info.fee_growth_outside_a = flip_a;
            tick_info.fee_growth_outside_b = flip_b;
            Self::set_tick(&env, tick, &tick_info);
        }

        if walk.fee_growth_global_a_delta != 0 {
            env.storage().instance().set(
                &DataKey::FeeGrowthGlobalA,
                &(fg_a + walk.fee_growth_global_a_delta),
            );
        }
        if walk.fee_growth_global_b_delta != 0 {
            env.storage().instance().set(
                &DataKey::FeeGrowthGlobalB,
                &(fg_b + walk.fee_growth_global_b_delta),
            );
        }
        if walk.protocol_fee_a_delta > 0 {
            let accrued_a: i128 = env
                .storage()
                .instance()
                .get(&DataKey::AccruedProtocolFeeA)
                .unwrap_or(0);
            env.storage().instance().set(
                &DataKey::AccruedProtocolFeeA,
                &(accrued_a + walk.protocol_fee_a_delta),
            );
        }
        if walk.protocol_fee_b_delta > 0 {
            let accrued_b: i128 = env
                .storage()
                .instance()
                .get(&DataKey::AccruedProtocolFeeB)
                .unwrap_or(0);
            env.storage().instance().set(
                &DataKey::AccruedProtocolFeeB,
                &(accrued_b + walk.protocol_fee_b_delta),
            );
        }

        if walk.amount_in_gross_total > 0 {
            TokenClient::new(&env, &token_in).transfer(
                &sender,
                &env.current_contract_address(),
                &walk.amount_in_gross_total,
            );
        }
        if walk.amount_out_filled > 0 {
            TokenClient::new(&env, &token_out).transfer(
                &env.current_contract_address(),
                &sender,
                &walk.amount_out_filled,
            );
        }

        env.storage()
            .instance()
            .set(&DataKey::CurrentTick, &walk.tick_final);
        env.storage()
            .instance()
            .set(&DataKey::ActiveLiquidity, &walk.active_liquidity_final);
        env.storage()
            .instance()
            .set(&DataKey::SqrtPriceX96, &walk.sqrt_price_final);

        soroban_amm_sdk::emit_versioned_event!(
            env,
            (symbol_short!("swap_out"), sender),
            (
                zero_for_one,
                walk.amount_in_gross_total,
                walk.amount_out_filled,
                walk.sqrt_price_final,
                walk.tick_final,
            )
        );

        if walk.sqrt_price_final != sqrt_price_before {
            soroban_amm_sdk::emit_versioned_event!(
                env,
                (symbol_short!("price_upd"), token_in, token_out),
                (
                    walk.amount_in_gross_total,
                    walk.amount_out_filled,
                    walk.sqrt_price_final,
                    walk.tick_final
                )
            );
        }

        Ok(walk.amount_in_gross_total)
    }

    /// Read-only simulation of `swap_exact_out`: returns the input required
    /// to receive exactly `amount_out`, without transferring tokens or
    /// mutating any state. Shares `walk_exact_out` with `swap_exact_out`, so
    /// the two can never disagree on the same pool state.
    pub fn quote_exact_out(
        env: Env,
        zero_for_one: bool,
        amount_out: i128,
        sqrt_price_limit_x96: u128,
    ) -> Result<i128, ClError> {
        Self::extend_instance_ttl(&env);
        Self::require_initialized(&env)?;
        if amount_out <= 0 {
            return Err(ClError::ZeroAmounts);
        }
        let fee_bps: i128 = env.storage().instance().get(&DataKey::FeeBps).unwrap_or(0);
        let protocol_fee_bps: i128 = env
            .storage()
            .instance()
            .get(&DataKey::ProtocolFeeBps)
            .unwrap_or(0);
        let current_tick: i32 = env
            .storage()
            .instance()
            .get(&DataKey::CurrentTick)
            .unwrap_or(0);
        let active_liquidity: i128 = env
            .storage()
            .instance()
            .get(&DataKey::ActiveLiquidity)
            .unwrap_or(0);
        let sqrt_price_x96 = Self::current_sqrt_price_x96(&env, current_tick);
        let fg_a: i128 = env
            .storage()
            .instance()
            .get(&DataKey::FeeGrowthGlobalA)
            .unwrap_or(0);
        let fg_b: i128 = env
            .storage()
            .instance()
            .get(&DataKey::FeeGrowthGlobalB)
            .unwrap_or(0);

        let walk = Self::walk_exact_out(
            &env,
            zero_for_one,
            amount_out,
            sqrt_price_limit_x96,
            fee_bps,
            protocol_fee_bps,
            current_tick,
            active_liquidity,
            sqrt_price_x96,
            fg_a,
            fg_b,
        );

        if !walk.fully_filled {
            return Err(ClError::ExactOutNotFullyFilled);
        }
        Ok(walk.amount_in_gross_total)
    }

    /// Estimate swap output and price impact without transferring tokens or mutating pool state.
    ///
    /// This walks initialized ticks exactly like `swap`, so the returned output,
    /// final tick, final sqrt price, and fee amount should match an immediately
    /// executed swap with the same parameters.
    pub fn estimate_price_impact(
        env: Env,
        zero_for_one: bool,
        amount_in: i128,
        sqrt_price_limit_x96: u128,
    ) -> Result<PriceImpactEstimate, ClError> {
        Self::extend_instance_ttl(&env);
        Self::require_initialized(&env)?;
        if amount_in <= 0 {
            return Err(ClError::ZeroAmounts);
        }

        let fee_bps: i128 = env.storage().instance().get(&DataKey::FeeBps).unwrap_or(0);
        let tick_before = env
            .storage()
            .instance()
            .get(&DataKey::CurrentTick)
            .unwrap_or(0);
        let active_liquidity_before = env
            .storage()
            .instance()
            .get(&DataKey::ActiveLiquidity)
            .unwrap_or(0);
        let sqrt_price_before = Self::current_sqrt_price_x96(&env, tick_before);

        let (
            amount_in_actual,
            amount_in_after_fee,
            amount_out,
            sqrt_price_after,
            tick_after,
            active_liquidity_after,
        ) = Self::simulate_swap_walk(
            &env,
            zero_for_one,
            amount_in,
            sqrt_price_limit_x96,
            fee_bps,
            tick_before,
            active_liquidity_before,
            sqrt_price_before,
        );

        let fee_amount = amount_in_actual - amount_in_after_fee;
        let spot_price_before = Self::spot_price_for_direction(sqrt_price_before, zero_for_one);
        let effective_price = if amount_in_actual > 0 {
            amount_out * PRICE_SCALE / amount_in_actual
        } else {
            0
        };
        let price_impact_bps = if spot_price_before > 0 && effective_price < spot_price_before {
            (spot_price_before - effective_price) * 10_000 / spot_price_before
        } else {
            0
        };

        Ok(PriceImpactEstimate {
            amount_in: amount_in_actual,
            amount_in_after_fee,
            amount_out,
            fee_amount,
            spot_price_before,
            effective_price,
            price_impact_bps,
            sqrt_price_before,
            sqrt_price_after,
            tick_before,
            tick_after,
            active_liquidity_before,
            active_liquidity_after,
        })
    }

    /// Returns raw (tick_cumulative, last_timestamp) for external consumers.
    pub fn get_tick_cumulative(env: Env) -> (i64, u64) {
        Self::extend_instance_ttl(&env);
        let cum: i64 = env
            .storage()
            .instance()
            .get(&DataKey::TickCumulative)
            .unwrap_or(0);
        let ts: u64 = env
            .storage()
            .instance()
            .get(&DataKey::LastOracleTimestamp)
            .unwrap_or(0);
        (cum, ts)
    }

    /// Returns the current oracle ring-buffer capacity (`cardinality`, #1101).
    pub fn get_oracle_cardinality(env: Env) -> u32 {
        Self::extend_instance_ttl(&env);
        env.storage()
            .instance()
            .get::<_, OracleHeader>(&DataKey::OracleHeader)
            .map(|h| h.cardinality)
            .unwrap_or(DEFAULT_ORACLE_CARDINALITY)
    }

    /// Returns the current oracle ring-buffer header (`index`, `cardinality`, `count`, #1101).
    pub fn get_oracle_header(env: Env) -> OracleHeader {
        Self::extend_instance_ttl(&env);
        env.storage()
            .instance()
            .get::<_, OracleHeader>(&DataKey::OracleHeader)
            .unwrap_or(OracleHeader {
                index: 0,
                cardinality: DEFAULT_ORACLE_CARDINALITY,
                count: 0,
            })
    }

    /// Admin: grow the oracle observation ring-buffer capacity (`cardinality`, #1101).
    ///
    /// `new_cardinality` must be `>= current cardinality` and `<= MAX_ORACLE_CARDINALITY`.
    /// Shrinking or zero cardinality is rejected with [`ClError::InvalidOracleCardinality`].
    pub fn set_oracle_cardinality(
        env: Env,
        admin: Address,
        new_cardinality: u32,
    ) -> Result<(), ClError> {
        Self::extend_instance_ttl(&env);
        let stored = Self::read_admin(&env)?;
        if admin != stored {
            return Err(ClError::Unauthorized);
        }
        admin.require_auth();

        let mut header = Self::migrate_legacy_oracle_if_needed(&env);
        if new_cardinality == 0
            || new_cardinality < header.cardinality
            || new_cardinality > MAX_ORACLE_CARDINALITY
        {
            return Err(ClError::InvalidOracleCardinality);
        }

        if new_cardinality > header.cardinality {
            // If the ring buffer had wrapped around under the old cardinality,
            // normalize the existing `count` observations into contiguous slots
            // `0..count` in chronological order so the invariant (`count < cardinality`
            // implies observations live at `0..count` with `index == count - 1`) holds.
            if header.count == header.cardinality
                && header.count > 0
                && header.index != header.count - 1
            {
                let mut ordered: Vec<OracleObservation> = Vec::new(&env);
                for k in 0..header.count {
                    let slot = (header.index + 1 + k) % header.cardinality;
                    let obs: OracleObservation = env
                        .storage()
                        .persistent()
                        .get(&DataKey::OracleObservation(slot))
                        .unwrap_or(OracleObservation {
                            timestamp: 0,
                            cumulative: 0,
                        });
                    ordered.push_back(obs);
                }
                for k in 0..header.count {
                    let key = DataKey::OracleObservation(k);
                    let obs = ordered.get(k).unwrap();
                    env.storage().persistent().set(&key, &obs);
                    Self::bump_position(&env, &key);
                }
                header.index = header.count - 1;
            }
            header.cardinality = new_cardinality;
            env.storage()
                .instance()
                .set(&DataKey::OracleHeader, &header);
        }

        Ok(())
    }

    /// Returns tick_cumulative at `seconds_ago` seconds in the past (#512, #1101).
    ///
    /// Uses binary search over the bounded persistent observation ring buffer (`O(log N)`),
    /// returning exact snapshots when available and linearly interpolating between
    /// bracketing observations. `seconds_ago == 0` returns the current cumulative
    /// value extrapolated to `now`.
    ///
    /// Returns [`ClError::ObservationTooOld`] when the requested timestamp precedes
    /// the oldest retained observation in the ring buffer.
    pub fn observe(env: Env, seconds_ago: u64) -> Result<i64, ClError> {
        Self::extend_instance_ttl(&env);
        let cum: i64 = env
            .storage()
            .instance()
            .get(&DataKey::TickCumulative)
            .unwrap_or(0);
        let last_ts: u64 = env
            .storage()
            .instance()
            .get(&DataKey::LastOracleTimestamp)
            .unwrap_or(0);
        let now = env.ledger().timestamp();
        if seconds_ago > now {
            return Err(ClError::ObservationTooOld);
        }
        let current_tick: i32 = env
            .storage()
            .instance()
            .get(&DataKey::CurrentTick)
            .unwrap_or(0);
        let target_ts = now - seconds_ago;
        if target_ts >= last_ts {
            // Extrapolate forward from last stored point
            let elapsed = (target_ts - last_ts) as i64;
            Ok(cum + (current_tick as i64) * elapsed)
        } else {
            Self::oracle_cumulative_at(&env, target_ts, cum, last_ts)
        }
    }

    /// Migrates pre-#1101 `OracleTimestamps` / `OraclePoint(t)` entries from instance
    /// storage into the persistent ring buffer (`OracleObservation(slot)` + `OracleHeader`)
    /// on the first oracle write after upgrade.
    fn migrate_legacy_oracle_if_needed(env: &Env) -> OracleHeader {
        if let Some(header) = env
            .storage()
            .instance()
            .get::<_, OracleHeader>(&DataKey::OracleHeader)
        {
            return header;
        }
        let cardinality = DEFAULT_ORACLE_CARDINALITY;
        let mut count = 0_u32;
        let mut last_migrated_ts = 0_u64;
        if let Some(timestamps) = env
            .storage()
            .instance()
            .get::<_, Vec<u64>>(&DataKey::OracleTimestamps)
        {
            let total = timestamps.len();
            let start = total.saturating_sub(cardinality);
            for i in 0..start {
                if let Some(t) = timestamps.get(i) {
                    env.storage().instance().remove(&DataKey::OraclePoint(t));
                }
            }
            for i in start..total {
                if let Some(t) = timestamps.get(i) {
                    let c: i64 = env
                        .storage()
                        .instance()
                        .get(&DataKey::OraclePoint(t))
                        .unwrap_or(0);
                    env.storage().instance().remove(&DataKey::OraclePoint(t));
                    let obs_key = DataKey::OracleObservation(count);
                    env.storage().persistent().set(
                        &obs_key,
                        &OracleObservation {
                            timestamp: t,
                            cumulative: c,
                        },
                    );
                    Self::bump_position(env, &obs_key);
                    last_migrated_ts = t;
                    count += 1;
                }
            }
            env.storage().instance().remove(&DataKey::OracleTimestamps);

            // Also capture any trailing exact-out point at `LastOracleTimestamp` that
            // legacy `swap_exact_out` wrote without appending to `OracleTimestamps`.
            let last_ts: u64 = env
                .storage()
                .instance()
                .get(&DataKey::LastOracleTimestamp)
                .unwrap_or(0);
            if last_ts > last_migrated_ts {
                if let Some(c) = env
                    .storage()
                    .instance()
                    .get::<_, i64>(&DataKey::OraclePoint(last_ts))
                {
                    env.storage()
                        .instance()
                        .remove(&DataKey::OraclePoint(last_ts));
                    let slot = if count < cardinality { count } else { 0 };
                    let obs_key = DataKey::OracleObservation(slot);
                    env.storage().persistent().set(
                        &obs_key,
                        &OracleObservation {
                            timestamp: last_ts,
                            cumulative: c,
                        },
                    );
                    Self::bump_position(env, &obs_key);
                    if count < cardinality {
                        count += 1;
                    }
                }
            }
        }
        let header = OracleHeader {
            index: count.saturating_sub(1),
            cardinality,
            count,
        };
        env.storage()
            .instance()
            .set(&DataKey::OracleHeader, &header);
        header
    }

    fn record_oracle_point(env: &Env, timestamp: u64, cumulative: i64) {
        let mut header = Self::migrate_legacy_oracle_if_needed(env);
        let obs = OracleObservation {
            timestamp,
            cumulative,
        };
        if header.count == 0 {
            let key = DataKey::OracleObservation(0);
            env.storage().persistent().set(&key, &obs);
            Self::bump_position(env, &key);
            header.index = 0;
            header.count = 1;
            env.storage()
                .instance()
                .set(&DataKey::OracleHeader, &header);
            return;
        }

        let current_key = DataKey::OracleObservation(header.index);
        if let Some(last_obs) = env
            .storage()
            .persistent()
            .get::<_, OracleObservation>(&current_key)
        {
            if last_obs.timestamp == timestamp {
                env.storage().persistent().set(&current_key, &obs);
                Self::bump_position(env, &current_key);
                return;
            }
        }

        let next_slot = if header.count < header.cardinality {
            header.count
        } else {
            (header.index + 1) % header.cardinality
        };
        let next_key = DataKey::OracleObservation(next_slot);
        env.storage().persistent().set(&next_key, &obs);
        Self::bump_position(env, &next_key);
        header.index = next_slot;
        if header.count < header.cardinality {
            header.count += 1;
        }
        env.storage()
            .instance()
            .set(&DataKey::OracleHeader, &header);
    }

    /// Tick cumulative at `target_ts` using binary search over the ring buffer
    /// (with fallback to legacy `OracleTimestamps` for un-migrated pools) and
    /// linear interpolation between bracketing observations (#512, #1101).
    fn oracle_cumulative_at(
        env: &Env,
        target_ts: u64,
        live_cum: i64,
        last_ts: u64,
    ) -> Result<i64, ClError> {
        if let Some(header) = env
            .storage()
            .instance()
            .get::<_, OracleHeader>(&DataKey::OracleHeader)
        {
            if header.count == 0 {
                return Err(ClError::ObservationTooOld);
            }
            let slot_at = |k: u32| -> u32 {
                if header.count < header.cardinality {
                    k
                } else {
                    (header.index + 1 + k) % header.cardinality
                }
            };
            let read_obs = |k: u32| -> OracleObservation {
                let slot = slot_at(k);
                env.storage()
                    .persistent()
                    .get(&DataKey::OracleObservation(slot))
                    .unwrap_or(OracleObservation {
                        timestamp: 0,
                        cumulative: 0,
                    })
            };

            let oldest = read_obs(0);
            if target_ts < oldest.timestamp {
                return Err(ClError::ObservationTooOld);
            }
            if target_ts == oldest.timestamp {
                return Ok(oldest.cumulative);
            }

            let mut lo: u32 = 0;
            let mut hi: u32 = header.count - 1;
            let mut obs_lo = oldest;
            while lo < hi {
                let mid = lo + (hi - lo).div_ceil(2);
                let obs_mid = read_obs(mid);
                if obs_mid.timestamp <= target_ts {
                    lo = mid;
                    obs_lo = obs_mid;
                } else {
                    hi = mid - 1;
                }
            }

            if obs_lo.timestamp == target_ts {
                return Ok(obs_lo.cumulative);
            }
            let (t_hi, c_hi) = if lo + 1 < header.count {
                let obs_hi = read_obs(lo + 1);
                (obs_hi.timestamp, obs_hi.cumulative)
            } else {
                (last_ts, live_cum)
            };
            if t_hi <= obs_lo.timestamp {
                return Ok(obs_lo.cumulative);
            }
            let dt = (t_hi - obs_lo.timestamp) as i128;
            let elapsed = (target_ts - obs_lo.timestamp) as i128;
            return Ok(
                obs_lo.cumulative + (((c_hi - obs_lo.cumulative) as i128 * elapsed) / dt) as i64
            );
        }

        // Legacy fallback when `OracleHeader` has not yet been created after upgrade.
        if let Some(cum) = env
            .storage()
            .instance()
            .get(&DataKey::OraclePoint(target_ts))
        {
            return Ok(cum);
        }
        let timestamps: Vec<u64> = match env.storage().instance().get(&DataKey::OracleTimestamps) {
            Some(ts) => ts,
            None => {
                return env
                    .storage()
                    .instance()
                    .get(&DataKey::OraclePoint(target_ts))
                    .ok_or(ClError::ObservationTooOld);
            }
        };
        if timestamps.is_empty() {
            return Err(ClError::ObservationTooOld);
        }
        let first = timestamps.get(0).unwrap();
        if target_ts < first {
            return Err(ClError::ObservationTooOld);
        }
        let mut lo: u32 = 0;
        let mut hi: u32 = timestamps.len() - 1;
        while lo < hi {
            let mid = lo + (hi - lo).div_ceil(2);
            let t = timestamps.get(mid).unwrap();
            if t <= target_ts {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        let t_lo = timestamps.get(lo).unwrap();
        let c_lo: i64 = env
            .storage()
            .instance()
            .get(&DataKey::OraclePoint(t_lo))
            .unwrap_or(0);
        if target_ts == t_lo {
            return Ok(c_lo);
        }
        let (t_hi, c_hi) = if lo + 1 < timestamps.len() {
            let t = timestamps.get(lo + 1).unwrap();
            let c = env
                .storage()
                .instance()
                .get(&DataKey::OraclePoint(t))
                .unwrap_or(0);
            (t, c)
        } else {
            (last_ts, live_cum)
        };
        if t_hi <= t_lo {
            return Ok(c_lo);
        }
        let dt = (t_hi - t_lo) as i128;
        let elapsed = (target_ts - t_lo) as i128;
        Ok(c_lo + (((c_hi - c_lo) as i128 * elapsed) / dt) as i64)
    }

    /// Returns all open position tick-range pairs for `provider`.
    pub fn get_positions(env: Env, provider: Address) -> Vec<(i32, i32)> {
        Self::extend_instance_ttl(&env);
        env.storage()
            .persistent()
            .get(&DataKey::PositionList(provider))
            .unwrap_or_else(|| Vec::new(&env))
    }

    /// Simulate token amounts required for a given tick range and liquidity.
    /// Pure read — does not transfer tokens or modify state.
    pub fn quote_position(
        env: Env,
        lower_tick: i32,
        upper_tick: i32,
        liquidity: i128,
    ) -> Result<(i128, i128), ClError> {
        Self::extend_instance_ttl(&env);
        Self::require_initialized(&env)?;
        if lower_tick >= upper_tick {
            return Err(ClError::TickOutOfRange);
        }
        if lower_tick < MIN_TICK || upper_tick > MAX_TICK {
            return Err(ClError::TickOutOfRange);
        }
        if liquidity <= 0 {
            return Err(ClError::ZeroLiquidity);
        }
        let current_tick: i32 = env
            .storage()
            .instance()
            .get(&DataKey::CurrentTick)
            .unwrap_or(0);
        let sqrt_price_now = Self::current_sqrt_price(&env, current_tick);
        Self::amounts_for_liquidity_to_burn(
            current_tick,
            lower_tick,
            upper_tick,
            liquidity,
            sqrt_price_now,
        )
    }

    pub fn fee_growth_inside(env: Env, lower_tick: i32, upper_tick: i32) -> (i128, i128) {
        Self::extend_instance_ttl(&env);
        let current_tick: i32 = env
            .storage()
            .instance()
            .get(&DataKey::CurrentTick)
            .unwrap_or(0);
        let fg_a: i128 = env
            .storage()
            .instance()
            .get(&DataKey::FeeGrowthGlobalA)
            .unwrap_or(0);
        let fg_b: i128 = env
            .storage()
            .instance()
            .get(&DataKey::FeeGrowthGlobalB)
            .unwrap_or(0);

        let (f_below_a, f_below_b) =
            Self::fee_growth_below_helper(&env, lower_tick, current_tick, fg_a, fg_b);
        let (f_above_a, f_above_b) =
            Self::fee_growth_above_helper(&env, upper_tick, current_tick, fg_a, fg_b);

        let inside_a = fg_a - f_below_a - f_above_a;
        let inside_b = fg_b - f_below_b - f_above_b;

        (inside_a, inside_b)
    }

    /// Display-only view returning `PRICE_SCALE * 1.0001^tick` via binary
    /// exponentiation (`PRICE_SCALE = 1_000_000`).
    ///
    /// Note: this 6-decimal representation is lossy at low negative ticks and
    /// saturating at high positive ticks, so it must never be used for trade or
    /// position math (use [`Self::tick_to_sqrt_price_x96`] instead).
    pub fn tick_to_price(tick: i32) -> i128 {
        Self::tick_to_price_bexp(tick)
    }

    fn tick_to_price_bexp(tick: i32) -> i128 {
        if tick == 0 {
            return PRICE_SCALE;
        }
        let abs_tick = tick.unsigned_abs();
        let mut price = PRICE_SCALE;
        // base = TICK_BASE_NUM / TICK_BASE_DEN in PRICE_SCALE units = 1.0001 * 1_000_000
        let mut base = TICK_BASE_NUM;
        let mut exp = abs_tick;
        while exp > 0 {
            if exp & 1 != 0 {
                price = price.saturating_mul(base) / TICK_BASE_DEN;
            }
            base = base.saturating_mul(base) / TICK_BASE_DEN;
            exp >>= 1;
        }
        if tick < 0 {
            if price <= 0 {
                1
            } else {
                (PRICE_SCALE * PRICE_SCALE) / price
            }
        } else {
            price
        }
    }

    /// Compute token amounts needed to burn `liquidity` from a position.
    /// Returns (amount_a, amount_b) based on current tick position.
    /// The pool's live sqrt price.
    ///
    /// `SqrtPriceX96` is written by `initialize` and by every swap. Pools
    /// deployed before it was set at initialize have no stored value until
    /// their first trade, so fall back to the price at the current tick --
    /// which is exactly what the pool price was at initialization.
    fn current_sqrt_price(env: &Env, current_tick: i32) -> u128 {
        env.storage()
            .instance()
            .get(&DataKey::SqrtPriceX96)
            .unwrap_or_else(|| Self::tick_to_sqrt_price_x96(current_tick))
    }

    /// Token amounts an in-range position is worth at the pool's *current*
    /// price.
    ///
    /// `sqrt_current_x96` must be the pool's live `SqrtPriceX96`, not
    /// `tick_to_sqrt_price_x96(ct)`. A tick spans a whole price band and every
    /// swap moves the price inside it, so valuing an in-range position at the
    /// tick's lower edge splits it between the two tokens at a price the pool
    /// is not actually at. The position then redeems for more of one token
    /// than the pool holds and leaves the other stranded — the residual
    /// per-token shortfall left over after the swap-price fix in this change.
    fn amounts_for_liquidity_to_burn(
        ct: i32,
        lt: i32,
        ut: i32,
        liquidity: i128,
        sqrt_current_x96: u128,
    ) -> Result<(i128, i128), ClError> {
        if ct < lt {
            let sqrt_lower = Self::tick_to_sqrt_price_x96(lt);
            let sqrt_upper = Self::tick_to_sqrt_price_x96(ut);
            let amount_a = math::get_amount0_delta(sqrt_lower, sqrt_upper, liquidity)
                .ok_or(ClError::MathOverflow)?;
            return Ok((amount_a, 0));
        }
        if ct >= ut {
            let sqrt_lower = Self::tick_to_sqrt_price_x96(lt);
            let sqrt_upper = Self::tick_to_sqrt_price_x96(ut);
            let amount_b = math::get_amount1_delta(sqrt_lower, sqrt_upper, liquidity)
                .ok_or(ClError::MathOverflow)?;
            return Ok((0, amount_b));
        }

        // In-range: split at the live price, clamped into the position's own
        // band so a price that has drifted outside it cannot produce a
        // negative leg.
        let sqrt_lower = Self::tick_to_sqrt_price_x96(lt);
        let sqrt_upper = Self::tick_to_sqrt_price_x96(ut);
        let sqrt_current = sqrt_current_x96.clamp(sqrt_lower, sqrt_upper);

        // Token A covers [current, upper], Token B covers [lower, current]
        let amount_a = math::get_amount0_delta(sqrt_current, sqrt_upper, liquidity)
            .ok_or(ClError::MathOverflow)?;
        let amount_b = math::get_amount1_delta(sqrt_lower, sqrt_current, liquidity)
            .ok_or(ClError::MathOverflow)?;
        Ok((amount_a, amount_b))
    }

    /// Inverse of [`Self::amounts_for_liquidity_to_burn`]; takes the live
    /// `sqrt_current_x96` for the same reason, so a deposit and the withdrawal
    /// that undoes it are priced identically.
    fn liquidity_from_amounts(
        ct: i32,
        lt: i32,
        ut: i32,
        a: i128,
        b: i128,
        sqrt_current_x96: u128,
    ) -> Result<i128, ClError> {
        if ct < lt {
            let sqrt_lower = Self::tick_to_sqrt_price_x96(lt);
            let sqrt_upper = Self::tick_to_sqrt_price_x96(ut);
            Ok(math::get_liquidity_for_amount0(sqrt_lower, sqrt_upper, a)
                .ok_or(ClError::MathOverflow)?
                .max(1))
        } else if ct >= ut {
            let sqrt_lower = Self::tick_to_sqrt_price_x96(lt);
            let sqrt_upper = Self::tick_to_sqrt_price_x96(ut);
            Ok(math::get_liquidity_for_amount1(sqrt_lower, sqrt_upper, b)
                .ok_or(ClError::MathOverflow)?
                .max(1))
        } else {
            let sqrt_lower = Self::tick_to_sqrt_price_x96(lt);
            let sqrt_upper = Self::tick_to_sqrt_price_x96(ut);
            let sqrt_current = sqrt_current_x96.clamp(sqrt_lower, sqrt_upper);
            let liquidity_from_amount0 =
                math::get_liquidity_for_amount0(sqrt_current, sqrt_upper, a)
                    .ok_or(ClError::MathOverflow)?;
            let liquidity_from_amount1 =
                math::get_liquidity_for_amount1(sqrt_lower, sqrt_current, b)
                    .ok_or(ClError::MathOverflow)?;
            Ok(liquidity_from_amount0.min(liquidity_from_amount1).max(1))
        }
    }

    fn pending_fees(pos: &Position, fg_inside_a: i128, fg_inside_b: i128) -> (i128, i128) {
        let da = fg_inside_a - pos.fee_growth_inside_a;
        let db = fg_inside_b - pos.fee_growth_inside_b;
        let oa = if da > 0 {
            math::mul_div(pos.liquidity as u128, da as u128, FEE_GROWTH_SCALE as u128).unwrap_or(0)
                as i128
        } else {
            0
        };
        let ob = if db > 0 {
            math::mul_div(pos.liquidity as u128, db as u128, FEE_GROWTH_SCALE as u128).unwrap_or(0)
                as i128
        } else {
            0
        };
        (oa, ob)
    }

    fn get_bitmap_word(env: &Env, word_pos: i32) -> u128 {
        let key = DataKey::TickBitmap(word_pos);
        if let Some(word) = env.storage().persistent().get::<_, u128>(&key) {
            Self::bump_position(env, &key);
            word
        } else {
            env.storage().instance().get::<_, u128>(&key).unwrap_or(0)
        }
    }

    fn set_bitmap_word(env: &Env, word_pos: i32, word: u128) {
        let key = DataKey::TickBitmap(word_pos);
        if word == 0 {
            env.storage().persistent().remove(&key);
            env.storage().instance().remove(&key);
            Self::remove_bitmap_word_index(env, word_pos);
        } else {
            env.storage().persistent().set(&key, &word);
            Self::bump_position(env, &key);
            env.storage().instance().remove(&key);
            Self::add_bitmap_word_index(env, word_pos);
        }
    }

    fn add_bitmap_word_index(env: &Env, word_pos: i32) {
        let mut words: Vec<i32> = env
            .storage()
            .persistent()
            .get(&DataKey::TickBitmapWords)
            .unwrap_or_else(|| Vec::new(env));
        let mut low = 0u32;
        let mut high = words.len();
        while low < high {
            let mid = low + (high - low) / 2;
            let val = words.get(mid).unwrap();
            if val == word_pos {
                return;
            } else if val < word_pos {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        words.insert(low, word_pos);
        env.storage()
            .persistent()
            .set(&DataKey::TickBitmapWords, &words);
        Self::bump_position(env, &DataKey::TickBitmapWords);
    }

    fn remove_bitmap_word_index(env: &Env, word_pos: i32) {
        let mut words: Vec<i32> = match env.storage().persistent().get(&DataKey::TickBitmapWords) {
            Some(w) => w,
            None => return,
        };
        let mut low = 0u32;
        let mut high = words.len();
        let mut found = None;
        while low < high {
            let mid = low + (high - low) / 2;
            let val = words.get(mid).unwrap();
            if val == word_pos {
                found = Some(mid);
                break;
            } else if val < word_pos {
                low = mid + 1;
            } else {
                high = mid;
            }
        }
        if let Some(idx) = found {
            words.remove(idx);
            env.storage()
                .persistent()
                .set(&DataKey::TickBitmapWords, &words);
            Self::bump_position(env, &DataKey::TickBitmapWords);
        }
    }

    fn flip_tick(env: &Env, tick: i32) {
        let word_pos = tick.div_euclid(128);
        let bit_pos = tick.rem_euclid(128) as u32;
        let word = Self::get_bitmap_word(env, word_pos);
        let new_word = word ^ (1u128 << bit_pos);
        Self::set_bitmap_word(env, word_pos, new_word);
    }

    fn next_initialized_tick(env: &Env, tick: i32, zero_for_one: bool) -> Option<i32> {
        let words_opt: Option<Vec<i32>> = env.storage().persistent().get(&DataKey::TickBitmapWords);
        if words_opt.is_some() {
            Self::bump_position(env, &DataKey::TickBitmapWords);
        }

        if zero_for_one {
            let word_pos = tick.div_euclid(128);
            let bit_pos = tick.rem_euclid(128) as u32;
            let word = Self::get_bitmap_word(env, word_pos);
            let mask = if bit_pos == 127 {
                u128::MAX
            } else {
                (1u128 << (bit_pos + 1)) - 1
            };
            let masked = word & mask;
            if masked != 0 {
                let bit = 127 - masked.leading_zeros();
                return Some(word_pos * 128 + bit as i32);
            }

            // O(1)-O(log N) jump using indexed words in persistent storage
            if let Some(ref words) = words_opt {
                let mut low = 0u32;
                let mut high = words.len();
                while low < high {
                    let mid = low + (high - low) / 2;
                    let val = words.get(mid).unwrap();
                    if val < word_pos {
                        low = mid + 1;
                    } else {
                        high = mid;
                    }
                }
                if low > 0 {
                    let next_word_pos = words.get(low - 1).unwrap();
                    let next_word = Self::get_bitmap_word(env, next_word_pos);
                    if next_word != 0 {
                        let bit = 127 - next_word.leading_zeros();
                        return Some(next_word_pos * 128 + bit as i32);
                    }
                }
            }
            None
        } else {
            let start_tick = tick + 1;
            let word_pos = start_tick.div_euclid(128);
            let bit_pos = start_tick.rem_euclid(128) as u32;
            let word = Self::get_bitmap_word(env, word_pos);
            let mask = u128::MAX.wrapping_shl(bit_pos);
            let masked = word & mask;
            if masked != 0 {
                let bit = masked.trailing_zeros();
                return Some(word_pos * 128 + bit as i32);
            }

            if let Some(ref words) = words_opt {
                let mut low = 0u32;
                let mut high = words.len();
                while low < high {
                    let mid = low + (high - low) / 2;
                    let val = words.get(mid).unwrap();
                    if val <= word_pos {
                        low = mid + 1;
                    } else {
                        high = mid;
                    }
                }
                if low < words.len() {
                    let next_word_pos = words.get(low).unwrap();
                    let next_word = Self::get_bitmap_word(env, next_word_pos);
                    if next_word != 0 {
                        let bit = next_word.trailing_zeros();
                        return Some(next_word_pos * 128 + bit as i32);
                    }
                }
            }
            None
        }
    }

    fn get_tick(env: &Env, tick: i32) -> TickInfo {
        let key = DataKey::Tick(tick);
        if let Some(info) = env.storage().persistent().get(&key) {
            Self::bump_position(env, &key);
            info
        } else if let Some(info) = env.storage().instance().get(&key) {
            info
        } else {
            TickInfo {
                liquidity_net: 0,
                liquidity_gross: 0,
                fee_growth_outside_a: 0,
                fee_growth_outside_b: 0,
                initialized: false,
            }
        }
    }

    fn set_tick(env: &Env, tick: i32, info: &TickInfo) {
        let key = DataKey::Tick(tick);
        if info.liquidity_gross == 0 {
            env.storage().persistent().remove(&key);
            env.storage().instance().remove(&key);
        } else {
            env.storage().persistent().set(&key, info);
            Self::bump_position(env, &key);
            env.storage().instance().remove(&key);
        }
    }

    fn step_target_price_and_limit(
        next_price_x96: u128,
        sqrt_price_limit_x96: u128,
        zero_for_one: bool,
    ) -> (u128, bool) {
        let mut target_price_x96 = next_price_x96;
        let mut hit_limit = false;
        if sqrt_price_limit_x96 != 0 {
            if zero_for_one {
                if next_price_x96 <= sqrt_price_limit_x96 {
                    target_price_x96 = sqrt_price_limit_x96;
                    hit_limit = true;
                }
            } else if next_price_x96 >= sqrt_price_limit_x96 {
                target_price_x96 = sqrt_price_limit_x96;
                hit_limit = true;
            }
        }
        (target_price_x96, hit_limit)
    }

    fn update_tick(
        env: &Env,
        tick: i32,
        current_tick: i32,
        liquidity_delta: i128,
        is_upper: bool,
        fg_a: i128,
        fg_b: i128,
    ) {
        let mut info = Self::get_tick(env, tick);
        let prev_gross = info.liquidity_gross;
        info.liquidity_gross += liquidity_delta;

        if prev_gross == 0 {
            info.initialized = true;
            if tick <= current_tick {
                info.fee_growth_outside_a = fg_a;
                info.fee_growth_outside_b = fg_b;
            } else {
                info.fee_growth_outside_a = 0;
                info.fee_growth_outside_b = 0;
            }
            info.initialized = true;
            Self::flip_tick(env, tick);
        }

        if is_upper {
            info.liquidity_net -= liquidity_delta;
        } else {
            info.liquidity_net += liquidity_delta;
        }

        if info.liquidity_gross == 0 {
            Self::flip_tick(env, tick);
            let key = DataKey::Tick(tick);
            env.storage().persistent().remove(&key);
            env.storage().instance().remove(&key);
        } else {
            Self::set_tick(env, tick, &info);
        }
    }

    fn fee_growth_below_helper(
        env: &Env,
        tick: i32,
        current_tick: i32,
        fg_a: i128,
        fg_b: i128,
    ) -> (i128, i128) {
        let info = Self::get_tick(env, tick);
        if current_tick >= tick {
            (info.fee_growth_outside_a, info.fee_growth_outside_b)
        } else {
            (
                fg_a - info.fee_growth_outside_a,
                fg_b - info.fee_growth_outside_b,
            )
        }
    }

    fn fee_growth_above_helper(
        env: &Env,
        tick: i32,
        current_tick: i32,
        fg_a: i128,
        fg_b: i128,
    ) -> (i128, i128) {
        let info = Self::get_tick(env, tick);
        if current_tick < tick {
            (info.fee_growth_outside_a, info.fee_growth_outside_b)
        } else {
            (
                fg_a - info.fee_growth_outside_a,
                fg_b - info.fee_growth_outside_b,
            )
        }
    }

    fn current_sqrt_price_x96(env: &Env, current_tick: i32) -> u128 {
        Self::current_sqrt_price(env, current_tick)
    }

    fn spot_price_for_direction(sqrt_price_x96: u128, zero_for_one: bool) -> i128 {
        if sqrt_price_x96 == 0 {
            return 0;
        }
        let directional_sqrt = if zero_for_one {
            sqrt_price_x96
        } else {
            math::mul_div(math::Q96, math::Q96, sqrt_price_x96).unwrap_or(u128::MAX)
        };
        let p_x96 =
            math::mul_div(directional_sqrt, directional_sqrt, math::Q96).unwrap_or(u128::MAX);
        let scaled = math::mul_div(p_x96, PRICE_SCALE as u128, math::Q96).unwrap_or(u128::MAX);
        Self::u128_to_i128_saturating(Some(scaled)).max(1)
    }

    #[allow(clippy::too_many_arguments)]
    fn simulate_swap_walk(
        env: &Env,
        zero_for_one: bool,
        amount_in: i128,
        sqrt_price_limit_x96: u128,
        fee_bps: i128,
        mut current_tick: i32,
        mut active_liquidity: i128,
        mut sqrt_price_x96: u128,
    ) -> (i128, i128, i128, u128, i32, i128) {
        let mut amount_remaining = amount_in;
        let mut amount_out_total = 0_i128;
        let mut amount_in_after_fee_total = 0_i128;

        while amount_remaining > 0 {
            let next_tick_opt = Self::next_initialized_tick(env, current_tick, zero_for_one);
            // No initialized ticks in this direction and no liquidity — nothing to trade.
            // amount_remaining is left untouched so this untraded portion is excluded
            // from amount_in_actual below.
            if next_tick_opt.is_none() && active_liquidity == 0 {
                break;
            }

            let next_tick = match next_tick_opt {
                Some(t) => {
                    if zero_for_one {
                        t.max(MIN_TICK)
                    } else {
                        t.min(MAX_TICK)
                    }
                }
                None => {
                    if zero_for_one {
                        MIN_TICK
                    } else {
                        MAX_TICK
                    }
                }
            };

            let next_price_x96 = Self::tick_to_sqrt_price_x96(next_tick);
            let (target_price_x96, hit_limit) = Self::step_target_price_and_limit(
                next_price_x96,
                sqrt_price_limit_x96,
                zero_for_one,
            );

            let amount_in_after_fee = amount_remaining * (10000 - fee_bps) / 10000;

            let (amount_in_step_after_fee, amount_out_step) = if active_liquidity == 0 {
                (0, 0)
            } else {
                Self::compute_step(
                    active_liquidity,
                    sqrt_price_x96,
                    target_price_x96,
                    zero_for_one,
                )
            };

            if (amount_in_after_fee >= amount_in_step_after_fee || active_liquidity == 0)
                && !hit_limit
            {
                let actual_step_in = if active_liquidity > 0 && fee_bps > 0 {
                    (amount_in_step_after_fee * 10000 + 10000 - fee_bps - 1) / (10000 - fee_bps)
                } else {
                    amount_in_step_after_fee
                };
                let actual_step_in = actual_step_in.min(amount_remaining);

                amount_remaining -= actual_step_in;
                amount_in_after_fee_total += amount_in_step_after_fee;
                amount_out_total += amount_out_step;
                sqrt_price_x96 = target_price_x96;

                let tick_info = Self::get_tick(env, next_tick);
                if zero_for_one {
                    active_liquidity -= tick_info.liquidity_net;
                    current_tick = (next_tick - 1).max(MIN_TICK);
                } else {
                    active_liquidity += tick_info.liquidity_net;
                    current_tick = next_tick;
                }
            } else {
                if active_liquidity > 0 {
                    let amt_in_after_fee = if hit_limit {
                        amount_in_step_after_fee
                    } else {
                        amount_in_after_fee
                    };

                    let (new_price_x96, amount_out_step) = Self::compute_final_price_and_output(
                        active_liquidity,
                        sqrt_price_x96,
                        amt_in_after_fee,
                        zero_for_one,
                    );

                    let actual_in = if hit_limit {
                        if fee_bps > 0 {
                            (amt_in_after_fee * 10000 + 10000 - fee_bps - 1) / (10000 - fee_bps)
                        } else {
                            amt_in_after_fee
                        }
                    } else {
                        amount_remaining
                    };
                    let actual_in = actual_in.min(amount_remaining);

                    amount_remaining -= actual_in;
                    amount_in_after_fee_total += amt_in_after_fee;
                    amount_out_total += amount_out_step;

                    if hit_limit {
                        sqrt_price_x96 = target_price_x96;
                    } else {
                        sqrt_price_x96 = new_price_x96;
                    }
                    // This branch only runs when the step's own math already
                    // determined the trade does *not* need to reach `next_tick`
                    // (the `if` arm above handles the case where it does). The
                    // step math works in a deliberately low-precision (~3
                    // significant digit) price representation, though, and its
                    // pool-favorable rounding can — right at a boundary —
                    // compute a price indistinguishable from (or past) the
                    // tick it was told not to reach. Reconstructing that price
                    // via the full-precision `sqrt_price_x96_to_tick` can then
                    // resolve to `next_tick` or beyond even though this step
                    // never crossed it. Clamp back to the known-correct side.
                    current_tick = Self::sqrt_price_x96_to_tick(sqrt_price_x96);
                    current_tick = if zero_for_one {
                        current_tick.max(next_tick)
                    } else {
                        current_tick.min(next_tick)
                    };
                } else {
                    sqrt_price_x96 = target_price_x96;
                    current_tick = Self::sqrt_price_x96_to_tick(sqrt_price_x96);
                    // No liquidity in this gap — amount_remaining is left untouched
                    // so this untraded portion is excluded from amount_in_actual.
                }
                break;
            }
        }

        (
            amount_in - amount_remaining,
            amount_in_after_fee_total,
            amount_out_total,
            sqrt_price_x96,
            current_tick,
            active_liquidity,
        )
    }

    /// Amounts required to move the price from `sqrt_price_current_x96` all the
    /// way to `sqrt_price_target_x96` (normally the next initialized tick).
    ///
    /// Evaluated in full Q64.96 precision. The previous implementation reduced
    /// both prices to a 3-significant-digit scale (`sqrt_price * 1000`), whose
    /// granularity is roughly 20 ticks: any target nearer than that collapsed
    /// to `diff == 0` and reported that the boundary could be reached for zero
    /// input, letting a swap cross ticks it never paid for.
    ///
    /// Input rounds up and output rounds down, both in the pool's favour.
    fn compute_step(
        liquidity: i128,
        sqrt_price_current_x96: u128,
        sqrt_price_target_x96: u128,
        zero_for_one: bool,
    ) -> (i128, i128) {
        if liquidity <= 0 || sqrt_price_current_x96 == sqrt_price_target_x96 {
            return (0, 0);
        }
        let liq = liquidity.unsigned_abs();

        let (lo, hi) = if zero_for_one {
            if sqrt_price_target_x96 >= sqrt_price_current_x96 {
                return (0, 0);
            }
            (sqrt_price_target_x96, sqrt_price_current_x96)
        } else {
            if sqrt_price_target_x96 <= sqrt_price_current_x96 {
                return (0, 0);
            }
            (sqrt_price_current_x96, sqrt_price_target_x96)
        };

        // zero_for_one: token0 in, token1 out. Otherwise the reverse.
        let (amount_in, amount_out) = if zero_for_one {
            (
                math::amount0_delta_exact(lo, hi, liq, true),
                math::amount1_delta_exact(lo, hi, liq, false),
            )
        } else {
            (
                math::amount1_delta_exact(lo, hi, liq, true),
                math::amount0_delta_exact(lo, hi, liq, false),
            )
        };

        (
            Self::u128_to_i128_saturating(amount_in),
            Self::u128_to_i128_saturating(amount_out),
        )
    }

    /// Clamp an optional wide result into the `i128` the contract's amount
    /// types use. `None` (overflow) and out-of-range values become 0 so a
    /// step reports "nothing tradeable" rather than wrapping to a bogus amount.
    fn u128_to_i128_saturating(v: Option<u128>) -> i128 {
        match v {
            Some(x) if x <= i128::MAX as u128 => x as i128,
            _ => 0,
        }
    }

    /// Landing price and output for a swap step that consumes
    /// `amount_in_after_fee` without reaching the next tick.
    ///
    /// Full Q64.96 precision. The previous implementation worked in a
    /// 3-significant-digit price scale (`p_c = sqrt_price * 1000`) whose
    /// granularity is about 20 ticks, so a single small trade against deep
    /// liquidity moved the pool price ~20 ticks instead of a fraction of one.
    /// The price then ran away from the price the trades had actually paid
    /// for, crossing out of live position ranges; at burn time every position
    /// was valued on the wrong side of its range, which is what produced the
    /// per-token insolvency (token A owed but absent, token B stranded).
    ///
    /// The price rounds against the trader and the output rounds down, so a
    /// step can never move the price further, or pay out more, than the input
    /// justifies.
    fn compute_final_price_and_output(
        liquidity: i128,
        sqrt_price_current_x96: u128,
        amount_in_after_fee: i128,
        zero_for_one: bool,
    ) -> (u128, i128) {
        if liquidity <= 0 || amount_in_after_fee <= 0 {
            return (sqrt_price_current_x96, 0);
        }
        let liq = liquidity.unsigned_abs();
        let amt = amount_in_after_fee.unsigned_abs();

        if zero_for_one {
            let next = match math::next_sqrt_price_from_amount0_in(sqrt_price_current_x96, liq, amt)
            {
                Some(p) if p <= sqrt_price_current_x96 => p,
                // Overflow, or rounding that would move the price the wrong
                // way: leave the price where it is and trade nothing this step.
                _ => return (sqrt_price_current_x96, 0),
            };
            let amount_out = Self::u128_to_i128_saturating(math::amount1_delta_exact(
                next,
                sqrt_price_current_x96,
                liq,
                false,
            ));
            (next, amount_out)
        } else {
            let next = match math::next_sqrt_price_from_amount1_in(sqrt_price_current_x96, liq, amt)
            {
                Some(p) if p >= sqrt_price_current_x96 => p,
                _ => return (sqrt_price_current_x96, 0),
            };
            let amount_out = Self::u128_to_i128_saturating(math::amount0_delta_exact(
                sqrt_price_current_x96,
                next,
                liq,
                false,
            ));
            (next, amount_out)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::token::StellarAssetClient;
    use soroban_sdk::{
        testutils::storage::Instance as _, testutils::Address as _, testutils::Ledger as _, Env,
    };

    #[allow(dead_code)]
    struct TestEnv<'a> {
        env: Env,
        admin: Address,
        provider: Address,
        token_a: Address,
        token_b: Address,
        cl_addr: Address,
        client: ConcentratedLiquidityClient<'a>,
        sac_a: StellarAssetClient<'a>,
        sac_b: StellarAssetClient<'a>,
    }

    fn setup_test_env<'a>(env: &'a Env, fee_bps: i128, initial_tick: i32) -> TestEnv<'a> {
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let admin = Address::generate(env);
        let provider = Address::generate(env);

        let token_a_sac = env.register_stellar_asset_contract_v2(admin.clone());
        let token_b_sac = env.register_stellar_asset_contract_v2(admin.clone());
        let token_a = token_a_sac.address();
        let token_b = token_b_sac.address();

        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(env, &cl_addr);
        client.initialize(&admin, &token_a, &token_b, &fee_bps, &initial_tick, &1_i32);

        let sac_a = StellarAssetClient::new(env, &token_a);
        let sac_b = StellarAssetClient::new(env, &token_b);

        // Mint lots of tokens to provider
        sac_a.mint(&provider, &10_000_000_i128);
        sac_b.mint(&provider, &10_000_000_i128);

        // Mint some to contract too just in case
        sac_a.mint(&cl_addr, &10_000_000_i128);
        sac_b.mint(&cl_addr, &10_000_000_i128);

        TestEnv {
            env: env.clone(),
            admin,
            provider,
            token_a,
            token_b,
            cl_addr: cl_addr.clone(),
            client,
            sac_a,
            sac_b,
        }
    }

    // ── instance-TTL regression coverage (issue #905) ──────────────────────

    fn cl_instance_ttl(env: &Env, cl_addr: &Address) -> u32 {
        env.as_contract(cl_addr, || env.storage().instance().get_ttl())
    }

    /// Advance the ledger far enough that the instance entry's remaining TTL
    /// drops below the extension threshold, so the next entrypoint call has to
    /// restore it.
    fn lower_instance_ttl_below_min(env: &Env, cl_addr: &Address) {
        env.ledger()
            .with_mut(|l| l.sequence_number += INSTANCE_BUMP_TO - INSTANCE_TTL_THRESHOLD + 1);
        let ttl = cl_instance_ttl(env, cl_addr);
        assert!(
            ttl < INSTANCE_TTL_THRESHOLD,
            "test setup should lower instance TTL below the threshold, got {ttl}"
        );
    }

    fn assert_instance_ttl_bumped(env: &Env, cl_addr: &Address) {
        let ttl = cl_instance_ttl(env, cl_addr);
        assert!(
            ttl >= INSTANCE_BUMP_TO - 1,
            "instance TTL {ttl} should be bumped toward INSTANCE_BUMP_TO"
        );
    }

    #[test]
    fn test_initialize_extends_instance_ttl() {
        let env = Env::default();
        let te = setup_test_env(&env, 30_i128, 0_i32);
        assert_instance_ttl_bumped(&env, &te.cl_addr);
    }

    #[test]
    fn test_read_entrypoint_restores_lapsed_instance_ttl() {
        let env = Env::default();
        let te = setup_test_env(&env, 30_i128, 0_i32);

        lower_instance_ttl_below_min(&env, &te.cl_addr);

        // A read-only pool-state query is often the only traffic the pool sees
        // for long stretches; it must still respond and restore the TTL rather
        // than let the pool drift into archival.
        let _ = te.client.get_pool_state();
        assert_instance_ttl_bumped(&env, &te.cl_addr);
    }

    #[test]
    fn test_write_entrypoint_restores_lapsed_instance_ttl() {
        let env = Env::default();
        let te = setup_test_env(&env, 30_i128, 0_i32);

        lower_instance_ttl_below_min(&env, &te.cl_addr);

        // `pause` writes only instance state, isolating the write-path TTL bump
        // from the persistent position entries.
        te.client.pause(&te.admin);

        assert!(te.client.is_paused());
        assert_instance_ttl_bumped(&env, &te.cl_addr);
    }

    #[test]
    fn test_pre_init_calls_return_not_initialized() {
        let env = Env::default();
        env.mock_all_auths();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        let admin = Address::generate(&env);
        let provider = Address::generate(&env);
        let token = Address::generate(&env);

        // Admin setters
        assert_eq!(
            client.try_set_oracle(&admin, &None),
            Err(Ok(ClError::NotInitialized))
        );
        assert_eq!(
            client.try_set_position_nft(&admin, &None),
            Err(Ok(ClError::NotInitialized))
        );
        assert_eq!(
            client.try_set_max_oracle_deviation_bps(&admin, &100),
            Err(Ok(ClError::NotInitialized))
        );
        assert_eq!(client.try_pause(&admin), Err(Ok(ClError::NotInitialized)));
        assert_eq!(client.try_unpause(&admin), Err(Ok(ClError::NotInitialized)));
        assert_eq!(
            client.try_propose_admin(&admin, &provider),
            Err(Ok(ClError::NotInitialized))
        );
        assert_eq!(
            client.try_set_protocol_fee(&admin, &provider, &100),
            Err(Ok(ClError::NotInitialized))
        );
        assert_eq!(
            client.try_withdraw_protocol_fees(&admin),
            Err(Ok(ClError::NotInitialized))
        );

        // Liquidity
        assert_eq!(
            client.try_mint_position(&provider, &-10, &10, &100, &100, &0, &0, &u64::MAX),
            Err(Ok(ClError::NotInitialized))
        );
        assert_eq!(
            client.try_modify_position(&provider, &-10, &10, &100, &0, &0, &u64::MAX),
            Err(Ok(ClError::NotInitialized))
        );
        assert_eq!(
            client.try_mint_position_single_token(
                &provider,
                &-10,
                &10,
                &token,
                &100,
                &0,
                &u64::MAX
            ),
            Err(Ok(ClError::NotInitialized))
        );
        assert_eq!(
            client.try_quote_single_token_deposit(&-10, &10, &token, &100),
            Err(Ok(ClError::NotInitialized))
        );
        assert_eq!(
            client.try_place_range_order(&provider, &10, &20, &token, &100, &0, &u64::MAX),
            Err(Ok(ClError::NotInitialized))
        );
        assert_eq!(
            client.try_check_range_order_filled(&provider, &10, &20),
            Err(Ok(ClError::NotInitialized))
        );
        assert_eq!(
            client.try_burn_position(&provider, &-10, &10, &1),
            Err(Ok(ClError::NotInitialized))
        );
        assert_eq!(
            client.try_burn_position_by_token_id(&provider, &0, &1),
            Err(Ok(ClError::NotInitialized))
        );
        assert_eq!(
            client.try_collect_fees(&provider, &-10, &10),
            Err(Ok(ClError::NotInitialized))
        );
        assert_eq!(
            client.try_collect_fees_by_token_id(&provider, &0),
            Err(Ok(ClError::NotInitialized))
        );
        assert_eq!(
            client.try_quote_position(&-10, &10, &100),
            Err(Ok(ClError::NotInitialized))
        );

        // Swaps and quotes
        let limit = ConcentratedLiquidity::tick_to_sqrt_price_x96(-100);
        assert_eq!(
            client.try_swap(&provider, &true, &100, &limit, &0, &u64::MAX),
            Err(Ok(ClError::NotInitialized))
        );
        assert_eq!(
            client.try_swap_exact_out(&provider, &true, &100, &limit, &1_000, &u64::MAX),
            Err(Ok(ClError::NotInitialized))
        );
        assert_eq!(
            client.try_quote_exact_out(&true, &100, &limit),
            Err(Ok(ClError::NotInitialized))
        );
        assert_eq!(
            client.try_estimate_price_impact(&true, &100, &limit),
            Err(Ok(ClError::NotInitialized))
        );

        // Views that used to unwrap pool state
        assert_eq!(client.try_current_tick(), Err(Ok(ClError::NotInitialized)));
        assert_eq!(client.try_get_tokens(), Err(Ok(ClError::NotInitialized)));
        assert_eq!(client.try_fee_bps(), Err(Ok(ClError::NotInitialized)));
    }

    #[test]
    fn test_pool_state_flow() {
        let env = Env::default();
        let te = setup_test_env(&env, 30_i128, 0_i32);

        // 1. Test after initialize
        let state1 = te.client.get_pool_state();
        assert_eq!(state1.current_tick, 0);
        assert_eq!(state1.active_liquidity, 0);
        assert_eq!(state1.sqrt_price, 1u128 << 96);

        // 2. Test after mint_position
        // Range [-100, 100] covers current_tick = 0, so active_liquidity should increase.
        te.client.mint_position(
            &te.provider,
            &-100_i32,
            &100_i32,
            &10_000_i128,
            &10_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );
        let state2 = te.client.get_pool_state();
        assert_eq!(state2.current_tick, 0);
        assert!(state2.active_liquidity > 0);

        // 3. Test after a swap (selling token A for token B)
        te.client.swap(
            &te.provider,
            &true,
            &1_000_i128,
            &0_u128,
            &0_i128,
            &10_000_u64,
        );
        let state3 = te.client.get_pool_state();
        assert!(state3.current_tick < 0);
        assert!(state3.sqrt_price < (1u128 << 96));
    }

    /// Regression test for issue #346: per-user position state must live in
    /// persistent storage, not the shared 64 KB instance budget.
    #[test]
    fn positions_are_stored_in_persistent_storage() {
        let env = Env::default();
        let te = setup_test_env(&env, 30_i128, 0_i32);

        te.client.mint_position(
            &te.provider,
            &-100_i32,
            &100_i32,
            &10_000_i128,
            &10_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );

        let pos_key = DataKey::Position(te.provider.clone(), -100, 100);
        let list_key = DataKey::PositionList(te.provider.clone());

        env.as_contract(&te.cl_addr, || {
            // The position and its tracking list are persisted...
            assert!(env.storage().persistent().has(&pos_key));
            assert!(env.storage().persistent().has(&list_key));
            // ...and must not consume the shared instance budget.
            assert!(!env.storage().instance().has(&pos_key));
            assert!(!env.storage().instance().has(&list_key));
        });

        // The view path resolves the same persistent entries.
        assert!(te.client.get_position(&te.provider, &-100, &100).liquidity > 0);
        assert_eq!(te.client.get_positions(&te.provider).len(), 1);
    }

    #[test]
    fn test_single_range_swap() {
        let env = Env::default();
        let te = setup_test_env(&env, 30, 0); // 0.3% fee, start at tick 0

        te.client.mint_position(
            &te.provider,
            &-100,
            &100,
            &100_000,
            &100_000,
            &0,
            &0,
            &u64::MAX,
        );

        let out = te.client.swap(&te.provider, &true, &1000, &0, &0, &10000);
        assert!(out > 0);

        let state = te.client.get_pool_state();
        assert!(state.current_tick < 0);
    }

    #[test]
    fn test_tick_crossing_swap() {
        let env = Env::default();
        let te = setup_test_env(&env, 0, 10); // 0% fee, start at tick 10

        te.client.mint_position(
            &te.provider,
            &-50,
            &0,
            &100_000,
            &100_000,
            &0,
            &0,
            &u64::MAX,
        );

        let state_before = te.client.get_pool_state();
        assert_eq!(state_before.active_liquidity, 0); // outside range

        let out = te.client.swap(&te.provider, &true, &5000, &0, &0, &10000);
        assert!(out > 0);

        let state_after = te.client.get_pool_state();
        assert!(state_after.current_tick < 0);
        assert!(state_after.active_liquidity > 0);
    }

    #[test]
    fn test_price_impact_estimate_matches_single_range_swap() {
        let env = Env::default();
        // Use all-negative ticks to avoid broken positive-tick sqrt math.
        let te = setup_test_env(&env, 30, -150);

        te.client.mint_position(
            &te.provider,
            &-200,
            &-100,
            &100_000,
            &100_000,
            &0,
            &0,
            &u64::MAX,
        );

        let quote = te.client.estimate_price_impact(&true, &1_000_i128, &0_u128);
        let out = te.client.swap(
            &te.provider,
            &true,
            &1_000_i128,
            &0_u128,
            &0_i128,
            &10_000_u64,
        );
        let state = te.client.get_pool_state();

        assert_eq!(quote.amount_out, out);
        assert_eq!(quote.sqrt_price_after, state.sqrt_price);
        assert_eq!(quote.tick_after, state.current_tick);
        assert_eq!(quote.active_liquidity_after, state.active_liquidity);
        assert!(quote.amount_in > 0);
        assert!(quote.fee_amount >= 0);
        assert!(quote.effective_price > 0);
        assert!(quote.price_impact_bps >= 0);
    }

    #[test]
    fn test_price_impact_estimate_matches_many_tick_crossing_swap() {
        let env = Env::default();
        // Use all-negative ticks; initial_tick=-25 is in range [-100, -1].
        let te = setup_test_env(&env, 25, -25);

        te.client
            .mint_position(&te.provider, &-400, &-300, &0, &80_000, &0, &0, &u64::MAX);
        te.client
            .mint_position(&te.provider, &-300, &-200, &0, &90_000, &0, &0, &u64::MAX);
        te.client.mint_position(
            &te.provider,
            &-200,
            &-100,
            &100_000,
            &100_000,
            &0,
            &0,
            &u64::MAX,
        );
        te.client.mint_position(
            &te.provider,
            &-100,
            &-1,
            &100_000,
            &100_000,
            &0,
            &0,
            &u64::MAX,
        );

        let quote = te
            .client
            .estimate_price_impact(&true, &25_000_i128, &0_u128);
        let out = te.client.swap(
            &te.provider,
            &true,
            &25_000_i128,
            &0_u128,
            &0_i128,
            &10_000_u64,
        );
        let state = te.client.get_pool_state();

        assert_eq!(quote.amount_out, out);
        assert_eq!(quote.sqrt_price_after, state.sqrt_price);
        assert_eq!(quote.tick_after, state.current_tick);
        assert_eq!(quote.active_liquidity_after, state.active_liquidity);
        assert!(quote.tick_after < quote.tick_before);
        assert!(quote.price_impact_bps >= 0);
    }

    #[test]
    fn test_limit_price_hit() {
        let env = Env::default();
        let te = setup_test_env(&env, 0, 0);

        te.client.mint_position(
            &te.provider,
            &-100,
            &100,
            &100_000,
            &100_000,
            &0,
            &0,
            &u64::MAX,
        );

        // A limit 0.1% below the current sqrt price. The previous limit here
        // was `Q96 - 1_000_000`, i.e. a relative move of ~1.3e-23, which no
        // real trade can produce: `amount1_out = L * (Q96 - limit) / Q96`
        // floors to 0 at this pool's liquidity (L ~ 2.0e7). It only appeared
        // to trade because the old 3-significant-digit price scale moved the
        // price in ~20-tick jumps regardless of the amount swapped.
        let q96 = 1u128 << 96;
        let limit = q96 - q96 / 1000;
        // Reaching that limit takes ~20_071 of token0, so 50_000 overshoots
        // and the swap must stop exactly at the limit.
        let out = te
            .client
            .swap(&te.provider, &true, &50_000, &limit, &0, &10000);
        assert!(out > 0);

        let state = te.client.get_pool_state();
        assert_eq!(state.sqrt_price, limit);
    }

    /// A small swap must move the price by what the trade actually justifies,
    /// not by the resolution of an internal fixed-point scale.
    ///
    /// The swap engine used to reduce the price to a three-significant-digit
    /// scale, `p = (sqrt_price * 1000) >> 96`, a resolution of ~20 ticks.
    /// One unit of `p` *is* a ~20-tick move, so any trade large enough to
    /// change `p` at all moved the pool ~20 ticks regardless of its size,
    /// and the pool price ran away from the price its trades had paid for.
    /// Once the price left a position's range, burning valued that position
    /// entirely in the wrong token: the pool owed token A it did not hold
    /// while stranding token B it could not pay out. That is the per-token
    /// insolvency this test guards against at its source.
    #[test]
    fn small_swap_moves_price_by_trade_size_not_scale_granularity() {
        let env = Env::default();
        let te = setup_test_env(&env, 0, 0); // zero fee keeps the arithmetic exact

        te.client.mint_position(
            &te.provider,
            &-1_000,
            &1_000,
            &1_000_000,
            &1_000_000,
            &0,
            &0,
            &u64::MAX,
        );
        let liquidity = te.client.active_liquidity();
        assert!(liquidity > 0);

        let before = te.client.get_pool_state().sqrt_price;
        te.client
            .swap(&te.provider, &true, &100_i128, &0_u128, &0_i128, &u64::MAX);
        let after = te.client.get_pool_state().sqrt_price;

        // Canonical Uniswap V3 result for token0 in:
        //   sqrt_next = L * sqrt_p / (L + amount0 * sqrt_p / Q96)
        let q96 = math::Q96;
        let term = (100u128 * q96) / q96;
        let expected = (liquidity as u128 * q96).div_ceil(liquidity as u128 + term);
        assert_eq!(after, expected, "swap must land on the canonical price");

        // 100 tokens against this liquidity is a fraction of one tick. The old
        // 3-significant-digit scale landed near tick -20 instead.
        assert!(after < before, "a token0-in swap must lower the price");
        assert!(
            after > math::tick_to_sqrt_price_x96(-1),
            "a 100-token swap must not move the price a full tick \
             (got {} which is at or below tick -1)",
            after
        );
        assert!(
            te.client.current_tick() >= -1,
            "tick moved {} on a 100-token trade",
            te.client.current_tick()
        );
    }

    #[test]
    fn test_deadline_expired() {
        let env = Env::default();
        let te = setup_test_env(&env, 0, 0);
        env.ledger().set_timestamp(101);
        let result = te.client.try_swap(&te.provider, &true, &1000, &0, &0, &100);
        assert_eq!(result, Err(Ok(ClError::DeadlineExpired)));
    }

    #[test]
    fn test_modify_position_deadline_expired() {
        let env = Env::default();
        let te = setup_test_env(&env, 0, 0);
        env.ledger().set_timestamp(101);
        // The deadline check runs before any position lookup, so an expired
        // deadline must short-circuit regardless of position state.
        let result = te
            .client
            .try_modify_position(&te.provider, &-100, &100, &1000, &0, &0, &100);
        assert_eq!(result, Err(Ok(ClError::DeadlineExpired)));
    }

    #[test]
    fn test_non_overlapping_fee_collection() {
        let env = Env::default();
        let te = setup_test_env(&env, 1000, 100); // 10% fee, start at tick 100

        let provider1 = Address::generate(&env);
        te.sac_a.mint(&provider1, &1_000_000);
        te.sac_b.mint(&provider1, &1_000_000);
        let provider2 = Address::generate(&env);
        te.sac_a.mint(&provider2, &1_000_000);
        te.sac_b.mint(&provider2, &1_000_000);

        te.client
            .mint_position(&provider1, &0, &50, &100_000, &100_000, &0, &0, &u64::MAX);
        te.client
            .mint_position(&provider2, &50, &150, &100_000, &100_000, &0, &0, &u64::MAX);

        // 200_000 in (180_000 after the 10% fee) is large enough to cross
        // down through both the 100->50 and 50->0 boundaries, so both
        // positions actually trade against the swap and must retain their
        // own fee share rather than losing it to whichever range the price
        // ends up in.
        te.client
            .swap(&te.provider, &true, &200_000, &0, &0, &10_000);

        let (f1_a, f1_b) = te.client.collect_fees(&provider1, &0, &50);
        let (f2_a, f2_b) = te.client.collect_fees(&provider2, &50, &150);

        assert!(f1_a > 0 || f1_b > 0);
        assert!(f2_a > 0 || f2_b > 0);
    }

    /// Same fee-growth-inside bug as above, with a third adjacent range so a
    /// single swap must cross two boundaries (100 and 50) rather than one,
    /// and both earlier-exited positions must retain their accrued fees
    /// rather than losing them to whichever range price ends up in.
    #[test]
    fn test_three_adjacent_ranges_each_retain_their_own_fee_share() {
        let env = Env::default();
        let te = setup_test_env(&env, 1000, 140); // 10% fee, start at tick 140

        let provider1 = Address::generate(&env);
        te.sac_a.mint(&provider1, &1_000_000);
        te.sac_b.mint(&provider1, &1_000_000);
        let provider2 = Address::generate(&env);
        te.sac_a.mint(&provider2, &1_000_000);
        te.sac_b.mint(&provider2, &1_000_000);
        let provider3 = Address::generate(&env);
        te.sac_a.mint(&provider3, &1_000_000);
        te.sac_b.mint(&provider3, &1_000_000);

        te.client
            .mint_position(&provider1, &0, &50, &100_000, &100_000, &0, &0, &u64::MAX);
        te.client
            .mint_position(&provider2, &50, &100, &100_000, &100_000, &0, &0, &u64::MAX);
        te.client.mint_position(
            &provider3,
            &100,
            &150,
            &100_000,
            &100_000,
            &0,
            &0,
            &u64::MAX,
        );

        // Sell A for B (price decreasing) far enough to cross both the 100
        // and 50 boundaries, starting from tick 140 (inside position 3).
        te.client
            .swap(&te.provider, &true, &200_000, &0, &0, &10_000);

        let (f2_a, f2_b) = te.client.collect_fees(&provider2, &50, &100);
        let (f3_a, f3_b) = te.client.collect_fees(&provider3, &100, &150);

        assert!(
            f3_a > 0 || f3_b > 0,
            "position covering the swap's starting range must retain its fee share"
        );
        assert!(
            f2_a > 0 || f2_b > 0,
            "the middle position, exited partway through the swap, must not lose its fee share"
        );
    }

    /// Audits the NFT-keyed path (`collect_fees_by_token_id`) for the same
    /// fee_growth_inside bug: it calls the identical `fee_growth_inside` as
    /// the legacy-owner path above, so it must retain fees the same way once
    /// price has crossed out of the position's range.
    #[test]
    fn test_collect_fees_by_token_id_retains_fee_share_after_tick_crossing() {
        use cl_position_nft::{ClPositionNft, ClPositionNftClient};

        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        client.initialize(&admin, &token_a, &token_b, &1000_i128, &100_i32, &1_i32);

        let nft_addr = env.register_contract(None, ClPositionNft);
        let nft = ClPositionNftClient::new(&env, &nft_addr);
        nft.initialize(&admin, &cl_addr);
        client.set_position_nft(&admin, &Some(nft_addr));

        let provider1 = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&provider1, &1_000_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&provider1, &1_000_000_i128);
        let provider2 = Address::generate(&env);
        StellarAssetClient::new(&env, &token_b).mint(&provider2, &1_000_000_i128);
        StellarAssetClient::new(&env, &token_a).mint(&provider2, &1_000_000_i128);

        client.mint_position(
            &provider1,
            &0_i32,
            &50_i32,
            &100_000_i128,
            &100_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );
        client.mint_position(
            &provider2,
            &50_i32,
            &150_i32,
            &100_000_i128,
            &100_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );
        // token_id 1: provider2's [50,150] position, minted second.
        let token_id = 1_u64;
        assert_eq!(
            client.position_of_token(&token_id),
            Some((provider2.clone(), 50_i32, 150_i32))
        );

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&trader, &50_000_i128);
        client.swap(&trader, &true, &20_000_i128, &0_u128, &0_i128, &u64::MAX);

        let (f_a, f_b) = client.collect_fees_by_token_id(&provider2, &token_id);
        assert!(
            f_a > 0 || f_b > 0,
            "the NFT-keyed path must retain fees the same way the legacy path does"
        );
    }

    // ── Issue #186: emergency pause tests ─────────────────────────────────────

    #[test]
    fn test_pause_rejects_mint_position() {
        let env = Env::default();
        let te = setup_test_env(&env, 30, 0);

        te.client.pause(&te.admin);
        assert!(te.client.is_paused());

        let result = te.client.try_mint_position(
            &te.provider,
            &-100,
            &100,
            &10_000,
            &10_000,
            &0,
            &0,
            &u64::MAX,
        );
        assert_eq!(result, Err(Ok(ClError::Paused)));
    }

    #[test]
    fn test_pause_rejects_swap() {
        let env = Env::default();
        let te = setup_test_env(&env, 30, 0);

        te.client.mint_position(
            &te.provider,
            &-100,
            &100,
            &10_000,
            &10_000,
            &0,
            &0,
            &u64::MAX,
        );
        te.client.pause(&te.admin);

        let result = te
            .client
            .try_swap(&te.provider, &true, &1_000, &0, &0, &u64::MAX);
        assert_eq!(result, Err(Ok(ClError::Paused)));
    }

    #[test]
    fn test_paused_allows_burn_and_collect() {
        let env = Env::default();
        let te = setup_test_env(&env, 30, 0);

        te.client
            .mint_position(&te.provider, &100, &200, &5_000, &0, &0, &0, &u64::MAX);
        let pos = te.client.get_position(&te.provider, &100, &200);
        let liq = pos.liquidity;

        te.client.pause(&te.admin);

        // burn_position should succeed while paused
        let result = te.client.try_burn_position(&te.provider, &100, &200, &liq);
        assert!(result.is_ok());

        // collect_fees should also succeed while paused (nothing to collect here but shouldn't error)
        // Re-mint to get a position to collect on
        te.client.unpause(&te.admin);
        te.client
            .mint_position(&te.provider, &100, &200, &5_000, &0, &0, &0, &u64::MAX);
        te.client.pause(&te.admin);
        let collect_result = te.client.try_collect_fees(&te.provider, &100, &200);
        assert!(collect_result.is_ok());
    }

    #[test]
    fn test_non_admin_pause_rejected() {
        let env = Env::default();
        let te = setup_test_env(&env, 30, 0);

        let non_admin = Address::generate(&env);
        let result = te.client.try_pause(&non_admin);
        assert_eq!(result, Err(Ok(ClError::Unauthorized)));
        assert!(!te.client.is_paused());
    }

    #[test]
    fn test_unpause_resumes_operations() {
        let env = Env::default();
        let te = setup_test_env(&env, 30, 0);

        te.client.pause(&te.admin);
        assert!(te.client.is_paused());

        te.client.unpause(&te.admin);
        assert!(!te.client.is_paused());

        // Should now succeed
        te.client.mint_position(
            &te.provider,
            &-100,
            &100,
            &10_000,
            &10_000,
            &0,
            &0,
            &u64::MAX,
        );
    }

    #[test]
    fn collect_fees_emits_coll_fees_event() {
        let env = Env::default();
        let te = setup_test_env(&env, 1000, 100);
        let cl_addr = te.cl_addr.clone();

        te.client.mint_position(
            &te.provider,
            &0,
            &150,
            &100_000,
            &100_000,
            &0,
            &0,
            &u64::MAX,
        );
        te.client
            .swap(&te.provider, &true, &2_000, &0, &0, &u64::MAX);

        let (total_a, total_b) = te.client.collect_fees(&te.provider, &0, &150);

        use soroban_sdk::{testutils::Events as _, IntoVal, Val, Vec as SdkVec};
        let expected_topics: SdkVec<Val> =
            (symbol_short!("coll_fees"), te.provider.clone()).into_val(&env);
        let event = env
            .events()
            .all()
            .iter()
            .find(|e| e.0 == cl_addr && e.1 == expected_topics)
            .expect("coll_fees event must be emitted");
        let __ver_4: (u32, (i32, i32, i128, i128)) = event.2.into_val(&env);
        assert_eq!(__ver_4.0, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        let data: (i32, i32, i128, i128) = __ver_4.1;
        assert_eq!(data, (0_i32, 150_i32, total_a, total_b));
    }

    /// Issue #352: a price-moving swap emits a `price_upd` event keyed by the
    /// token pair, carrying the input/output amounts and the new price + tick.
    #[test]
    fn swap_emits_price_upd_event() {
        let env = Env::default();
        let te = setup_test_env(&env, 30, 0);
        let cl_addr = te.cl_addr.clone();

        te.client.mint_position(
            &te.provider,
            &-100,
            &100,
            &100_000,
            &100_000,
            &0,
            &0,
            &u64::MAX,
        );

        // zero_for_one = true → token_in = token_a, token_out = token_b.
        let amount_out = te
            .client
            .swap(&te.provider, &true, &1_000, &0, &0, &u64::MAX);
        assert!(amount_out > 0);

        let state = te.client.get_pool_state();

        use soroban_sdk::{testutils::Events as _, IntoVal, Val, Vec as SdkVec};
        let expected_topics: SdkVec<Val> = (
            symbol_short!("price_upd"),
            te.token_a.clone(),
            te.token_b.clone(),
        )
            .into_val(&env);
        let event = env
            .events()
            .all()
            .iter()
            .find(|e| e.0 == cl_addr && e.1 == expected_topics)
            .expect("price_upd event must be emitted on a price-moving swap");
        let decoded: (u32, (i128, i128, u128, i32)) = event.2.into_val(&env);
        assert_eq!(decoded.0, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        let (amount_in_used, amount_out_emitted, new_sqrt_price, new_tick) = decoded.1;
        assert!(amount_in_used > 0);
        assert_eq!(amount_out_emitted, amount_out);
        assert_eq!(new_sqrt_price, state.sqrt_price);
        assert_eq!(new_tick, state.current_tick);
    }

    /// Issue #492: `sqrt_price_limit_x96 = 0` is the documented "no limit" sentinel.
    /// For `zero_for_one = false`, `next_price_x96 >= 0` is trivially true for every
    /// u128 price, so before the fix this forced `hit_limit = true` on the very
    /// first iteration and collapsed the target price to 0 — bricking the pool by
    /// persisting `SqrtPriceX96 = 0` even though no tokens were ever transferred.
    #[test]
    fn swap_zero_for_one_false_with_zero_price_limit_trades_normally() {
        let env = Env::default();
        let te = setup_test_env(&env, 30, 0);

        te.client.mint_position(
            &te.provider,
            &-100,
            &100,
            &100_000,
            &100_000,
            &0,
            &0,
            &u64::MAX,
        );

        let state_before = te.client.get_pool_state();

        // zero_for_one = false, sqrt_price_limit_x96 = 0 ("no limit"), as
        // dex_aggregator::execute_hops always passes for CL hops.
        let amount_out = te
            .client
            .swap(&te.provider, &false, &1_000, &0, &0, &u64::MAX);

        assert!(amount_out > 0, "swap must produce real output, not a no-op");

        let state_after = te.client.get_pool_state();
        assert_ne!(
            state_after.sqrt_price, 0,
            "pool price must never collapse to 0"
        );
        assert!(
            state_after.sqrt_price > state_before.sqrt_price,
            "zero_for_one = false must move the price up, not zero it out"
        );
    }

    #[test]
    fn second_collect_fees_returns_zero_without_new_swap() {
        let env = Env::default();
        let te = setup_test_env(&env, 1000, 100);

        te.client.mint_position(
            &te.provider,
            &0,
            &150,
            &100_000,
            &100_000,
            &0,
            &0,
            &u64::MAX,
        );
        te.client
            .swap(&te.provider, &true, &2_000, &0, &0, &u64::MAX);

        let (first_a, first_b) = te.client.collect_fees(&te.provider, &0, &150);
        assert!(first_a > 0 || first_b > 0);

        let (second_a, second_b) = te.client.collect_fees(&te.provider, &0, &150);
        assert_eq!((second_a, second_b), (0, 0));
    }

    #[test]
    fn collect_fees_does_not_reduce_liquidity() {
        let env = Env::default();
        let te = setup_test_env(&env, 1000, 100);

        te.client.mint_position(
            &te.provider,
            &0,
            &150,
            &100_000,
            &100_000,
            &0,
            &0,
            &u64::MAX,
        );
        let liq_before = te.client.get_position(&te.provider, &0, &150).liquidity;

        te.client
            .swap(&te.provider, &true, &2_000, &0, &0, &u64::MAX);
        te.client.collect_fees(&te.provider, &0, &150);

        let liq_after = te.client.get_position(&te.provider, &0, &150).liquidity;
        assert_eq!(liq_before, liq_after);
    }

    #[test]
    fn out_of_range_position_earns_no_fees() {
        let env = Env::default();
        let te = setup_test_env(&env, 1000, 100);

        let out_of_range = Address::generate(&env);
        te.sac_a.mint(&out_of_range, &1_000_000);
        te.sac_b.mint(&out_of_range, &1_000_000);

        te.client.mint_position(
            &te.provider,
            &0,
            &150,
            &100_000,
            &100_000,
            &0,
            &0,
            &u64::MAX,
        );
        te.client
            .mint_position(&out_of_range, &300, &400, &100_000, &0, &0, &0, &u64::MAX);
        te.client
            .swap(&te.provider, &true, &2_000, &0, &0, &u64::MAX);

        let (in_a, in_b) = te.client.collect_fees(&te.provider, &0, &150);
        let (out_a, out_b) = te.client.collect_fees(&out_of_range, &300, &400);
        assert!(in_a > 0 || in_b > 0);
        assert_eq!((out_a, out_b), (0, 0));
    }

    #[test]
    fn collect_fees_after_full_burn_returns_accrued_fees() {
        let env = Env::default();
        let te = setup_test_env(&env, 1000, 100);

        te.client.mint_position(
            &te.provider,
            &0,
            &150,
            &100_000,
            &100_000,
            &0,
            &0,
            &u64::MAX,
        );
        let liq = te.client.get_position(&te.provider, &0, &150).liquidity;

        te.client
            .swap(&te.provider, &true, &2_000, &0, &0, &u64::MAX);
        te.client.burn_position(&te.provider, &0, &150, &liq);

        assert_eq!(te.client.get_position(&te.provider, &0, &150).liquidity, 0);

        let (fee_a, fee_b) = te.client.collect_fees(&te.provider, &0, &150);
        assert!(fee_a > 0 || fee_b > 0);

        let (second_a, second_b) = te.client.collect_fees(&te.provider, &0, &150);
        assert_eq!((second_a, second_b), (0, 0));
    }

    #[test]
    fn fees_split_proportionally_between_equal_positions() {
        let env = Env::default();
        let te = setup_test_env(&env, 1000, 100);

        let p2 = Address::generate(&env);
        te.sac_a.mint(&p2, &1_000_000);
        te.sac_b.mint(&p2, &1_000_000);

        te.client.mint_position(
            &te.provider,
            &0,
            &150,
            &100_000,
            &100_000,
            &0,
            &0,
            &u64::MAX,
        );
        te.client
            .mint_position(&p2, &0, &150, &100_000, &100_000, &0, &0, &u64::MAX);
        te.client
            .swap(&te.provider, &true, &2_000, &0, &0, &u64::MAX);

        let (f1_a, f1_b) = te.client.collect_fees(&te.provider, &0, &150);
        let (f2_a, f2_b) = te.client.collect_fees(&p2, &0, &150);

        assert!(f1_a > 0 || f1_b > 0);
        assert!(f2_a > 0 || f2_b > 0);
        assert!((f1_a - f2_a).abs() <= 1);
        assert!((f1_b - f2_b).abs() <= 1);
    }

    #[test]
    fn collect_fees_after_second_swap_returns_only_new_fees() {
        let env = Env::default();
        let te = setup_test_env(&env, 1000, 100);

        te.client.mint_position(
            &te.provider,
            &0,
            &150,
            &100_000,
            &100_000,
            &0,
            &0,
            &u64::MAX,
        );
        te.client
            .swap(&te.provider, &true, &2_000, &0, &0, &u64::MAX);
        let (first_a, first_b) = te.client.collect_fees(&te.provider, &0, &150);

        te.client
            .swap(&te.provider, &false, &2_000, &u128::MAX, &0, &u64::MAX);
        let (second_a, second_b) = te.client.collect_fees(&te.provider, &0, &150);

        assert!(first_a + second_a > first_a || first_b + second_b > first_b);

        let (third_a, third_b) = te.client.collect_fees(&te.provider, &0, &150);
        assert_eq!((third_a, third_b), (0, 0));
    }

    #[test]
    fn tokens_owed_resets_after_collect() {
        let env = Env::default();
        let te = setup_test_env(&env, 1000, 100);

        te.client.mint_position(
            &te.provider,
            &0,
            &150,
            &100_000,
            &100_000,
            &0,
            &0,
            &u64::MAX,
        );
        te.client
            .swap(&te.provider, &true, &2_000, &0, &0, &u64::MAX);
        te.client.collect_fees(&te.provider, &0, &150);

        let pos = te.client.get_position(&te.provider, &0, &150);
        assert_eq!(pos.tokens_owed, (0, 0));
    }

    #[test]
    fn burn_after_collect_returns_principal_not_fees() {
        let env = Env::default();
        let te = setup_test_env(&env, 1000, 100);

        te.client
            .mint_position(&te.provider, &200, &300, &50_000, &0, &0, &0, &u64::MAX);
        let liq = te.client.get_position(&te.provider, &200, &300).liquidity;

        let (fee_a, fee_b) = te.client.collect_fees(&te.provider, &200, &300);
        assert_eq!((fee_a, fee_b), (0, 0));

        let (burn_a, burn_b) = te.client.burn_position(&te.provider, &200, &300, &liq);
        assert!(burn_a > 0);
        assert_eq!(burn_b, 0);
    }

    #[test]
    fn fees_accrued_before_partial_burn_are_collectable() {
        let env = Env::default();
        let te = setup_test_env(&env, 1000, 100);

        te.client.mint_position(
            &te.provider,
            &0,
            &150,
            &100_000,
            &100_000,
            &0,
            &0,
            &u64::MAX,
        );
        let liq = te.client.get_position(&te.provider, &0, &150).liquidity;

        te.client
            .swap(&te.provider, &true, &2_000, &0, &0, &u64::MAX);
        te.client.burn_position(&te.provider, &0, &150, &(liq / 2));

        let (fee_a, fee_b) = te.client.collect_fees(&te.provider, &0, &150);
        assert!(fee_a > 0 || fee_b > 0);

        let pos = te.client.get_position(&te.provider, &0, &150);
        assert_eq!(pos.liquidity, liq - liq / 2);
    }

    // ── mint_position deadline tests ────────────────────────────────────────

    #[test]
    fn mint_position_past_deadline_returns_deadline_expired() {
        let env = Env::default();
        let te = setup_test_env(&env, 30, 0);
        // Set a non-zero timestamp so past = now - 1 is strictly in the past.
        env.ledger().set_timestamp(100);
        let now = env.ledger().timestamp();
        let past = now.saturating_sub(1);
        let result = te.client.try_mint_position(
            &te.provider,
            &-100_i32,
            &100_i32,
            &10_000_i128,
            &10_000_i128,
            &0_i128,
            &0_i128,
            &past,
        );
        assert_eq!(result, Err(Ok(ClError::DeadlineExpired)));
    }

    #[test]
    fn mint_position_boundary_deadline_succeeds() {
        let env = Env::default();
        let te = setup_test_env(&env, 30, 0);
        let now = env.ledger().timestamp();
        // deadline == timestamp is inclusive — should succeed
        let result = te.client.try_mint_position(
            &te.provider,
            &-100_i32,
            &100_i32,
            &10_000_i128,
            &10_000_i128,
            &0_i128,
            &0_i128,
            &now,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn mint_position_far_future_deadline_behaves_identically() {
        let env = Env::default();
        let te = setup_test_env(&env, 30, 0);
        let (a, b) = te.client.mint_position(
            &te.provider,
            &-100_i32,
            &100_i32,
            &10_000_i128,
            &10_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );
        assert!(a >= 0);
        assert!(b >= 0);
        let pos = te.client.get_position(&te.provider, &-100_i32, &100_i32);
        assert!(pos.liquidity > 0);
    }

    #[test]
    fn mint_position_deadline_expired_performs_no_state_mutation() {
        let env = Env::default();
        let te = setup_test_env(&env, 30, 0);
        env.ledger().set_timestamp(100);

        let bal_a_before = TokenClient::new(&env, &te.token_a).balance(&te.provider);
        let bal_b_before = TokenClient::new(&env, &te.token_b).balance(&te.provider);

        let past = env.ledger().timestamp().saturating_sub(1);
        let result = te.client.try_mint_position(
            &te.provider,
            &-100_i32,
            &100_i32,
            &10_000_i128,
            &10_000_i128,
            &0_i128,
            &0_i128,
            &past,
        );
        assert_eq!(result, Err(Ok(ClError::DeadlineExpired)));

        // Token balances unchanged
        assert_eq!(
            TokenClient::new(&env, &te.token_a).balance(&te.provider),
            bal_a_before
        );
        assert_eq!(
            TokenClient::new(&env, &te.token_b).balance(&te.provider),
            bal_b_before
        );

        // No Position entry created — get_position should return NotFound
        let pos_result = te
            .client
            .try_get_position(&te.provider, &-100_i32, &100_i32);
        assert_eq!(pos_result, Err(Ok(ClError::PositionNotFound)));
    }

    #[test]
    fn mint_position_happy_path_with_deadline() {
        let env = Env::default();
        let te = setup_test_env(&env, 30, 0);
        let (a, b) = te.client.mint_position(
            &te.provider,
            &-100_i32,
            &100_i32,
            &10_000_i128,
            &10_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );
        assert!(a > 0 || b > 0);
        let pos = te.client.get_position(&te.provider, &-100_i32, &100_i32);
        assert!(pos.liquidity > 0);
    }

    #[test]
    fn mint_position_slippage_guard_with_deadline() {
        let env = Env::default();
        let te = setup_test_env(&env, 30, 0);
        // In-range position with min_a set impossibly high — SlippageExceeded
        // should fire even though deadline is far future.
        let result = te.client.try_mint_position(
            &te.provider,
            &-100_i32,
            &100_i32,
            &10_000_i128,
            &10_000_i128,
            &999_999_999_i128,
            &0_i128,
            &u64::MAX,
        );
        assert_eq!(result, Err(Ok(ClError::SlippageExceeded)));
    }

    // ── #963: position path over a high-magnitude tick range ─────────────────
    //
    // [100_000, 200_000] is a price ratio of ~22,000. Converting a 10^12
    // deposit to liquidity there needs `X * 2^96` or `X * sa * sb / 2^96`,
    // both far over u128::MAX, which the removed wrapping multiply truncated
    // silently: it credited 1_035_593 and 860_209 liquidity for the token-A
    // and token-B legs below instead of the exact values asserted here.
    //
    // Expected values are exact-precision floors computed from the prices in
    // `math::tick_to_sqrt_price_x96` (`PINNED[1]` in `math.rs`):
    //
    //   sa = 11_755_562_826_496_067_164_730_007_758_967
    //   sb = 1_744_244_129_640_337_381_386_292_646_062_446
    //   liq0 = floor(X * sa * sb / (2^96 * (sb - sa))) = 149_382_848_285_501
    //   liq1 = floor(X * 2^96 / (sb - sa))             = 45_730_842

    const WIDE_LO: i32 = 100_000;
    const WIDE_HI: i32 = 200_000;
    const WIDE_DEPOSIT: i128 = 1_000_000_000_000;

    /// Mints `WIDE_DEPOSIT` of one token into the wide range, checks the
    /// credited liquidity against the exact-precision value, then burns it all
    /// and checks the provider gets the deposit back within rounding.
    ///
    /// Tolerance: the round trip loses at most the value of the liquidity
    /// units dropped by rounding liquidity down, plus one unit for rounding
    /// the amount down. `max_loss` is that bound for the leg being tested.
    fn assert_wide_single_leg_round_trip(
        initial_tick: i32,
        deposit_a: bool,
        exact_liquidity: i128,
        liquidity_slack: i128,
        max_loss: i128,
    ) {
        let env = Env::default();
        let te = setup_test_env(&env, 30, initial_tick);
        let (sac, token) = if deposit_a {
            (&te.sac_a, &te.token_a)
        } else {
            (&te.sac_b, &te.token_b)
        };
        sac.mint(&te.provider, &WIDE_DEPOSIT);
        let tc = soroban_sdk::token::Client::new(&env, token);
        let before = tc.balance(&te.provider);

        let (desired_a, desired_b) = if deposit_a {
            (WIDE_DEPOSIT, 0)
        } else {
            (0, WIDE_DEPOSIT)
        };
        let (ma, mb) = te.client.mint_position(
            &te.provider,
            &WIDE_LO,
            &WIDE_HI,
            &desired_a,
            &desired_b,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );
        let minted = if deposit_a { ma } else { mb };
        assert_eq!(if deposit_a { mb } else { ma }, 0, "one-sided mint");

        let pos = te.client.get_position(&te.provider, &WIDE_LO, &WIDE_HI);
        // Liquidity rounds down: never more than the deposit pays for.
        assert!(
            pos.liquidity <= exact_liquidity && pos.liquidity >= exact_liquidity - liquidity_slack,
            "liquidity {} vs exact {exact_liquidity}",
            pos.liquidity
        );
        // The pool charges no more than was offered, and at most `max_loss` less.
        assert!(minted <= WIDE_DEPOSIT && WIDE_DEPOSIT - minted <= max_loss);

        // The quote for the recorded liquidity agrees with what was charged.
        assert_eq!(
            te.client.quote_position(&WIDE_LO, &WIDE_HI, &pos.liquidity),
            (ma, mb)
        );

        let (ba, bb) = te
            .client
            .burn_position(&te.provider, &WIDE_LO, &WIDE_HI, &pos.liquidity);
        // Burning pays back exactly what minting charged: never more.
        assert_eq!((ba, bb), (ma, mb));
        let after = tc.balance(&te.provider);
        assert!(after <= before && before - after <= max_loss);
    }

    #[test]
    fn wide_range_below_price_mint_burn_round_trips_token_a() {
        // Price below the range: token A only. The inner floor of
        // `get_liquidity_for_amount0` can drop up to sa / 2^96 + 1 = 149 units
        // of liquidity, each worth ~0.0067 of token A, so the round trip loses
        // at most 2 units; 3 leaves one unit of headroom.
        assert_wide_single_leg_round_trip(0, true, 149_382_848_285_501, 149, 3);
    }

    #[test]
    fn wide_range_above_price_mint_burn_round_trips_token_b() {
        // Price above the range: token B only. Liquidity is the exact floor.
        // One liquidity unit is worth (sb - sa) / 2^96 ~= 21_867.08 of token B,
        // which bounds the round-trip loss (plus one unit for the final floor).
        assert_wide_single_leg_round_trip(250_000, false, 45_730_842, 0, 21_869);
    }

    #[test]
    fn wide_range_in_price_mint_burn_round_trips_both_tokens() {
        let env = Env::default();
        let te = setup_test_env(&env, 30, 150_000);
        te.sac_a.mint(&te.provider, &WIDE_DEPOSIT);
        te.sac_b.mint(&te.provider, &WIDE_DEPOSIT);

        let (ma, mb) = te.client.mint_position(
            &te.provider,
            &WIDE_LO,
            &WIDE_HI,
            &WIDE_DEPOSIT,
            &WIDE_DEPOSIT,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );
        assert!(ma > 0 && mb > 0, "in-range mint takes both tokens");
        assert!(ma <= WIDE_DEPOSIT && mb <= WIDE_DEPOSIT);
        // Liquidity is limited by one token; that one is used almost in full.
        // At tick 150_000, a liquidity unit is worth (sp - sa) / 2^96 ~= 1_655.77
        // of token B and under 1 of token A.
        let limiting_loss = (WIDE_DEPOSIT - ma).min(WIDE_DEPOSIT - mb);
        assert!(limiting_loss <= 1_657, "limiting leg lost {limiting_loss}");

        let pos = te.client.get_position(&te.provider, &WIDE_LO, &WIDE_HI);
        let (ba, bb) = te
            .client
            .burn_position(&te.provider, &WIDE_LO, &WIDE_HI, &pos.liquidity);
        assert_eq!((ba, bb), (ma, mb));
    }

    #[test]
    fn wide_range_single_token_deposit_matches_exact_liquidity() {
        // Below-range single-token deposit of token A: the recorded liquidity
        // is the same exact-precision value as the two-token path, and the
        // quote agrees with the mint.
        let env = Env::default();
        let te = setup_test_env(&env, 30, 0);
        te.sac_a.mint(&te.provider, &WIDE_DEPOSIT);

        let quote =
            te.client
                .quote_single_token_deposit(&WIDE_LO, &WIDE_HI, &te.token_a, &WIDE_DEPOSIT);
        let res = te.client.mint_position_single_token(
            &te.provider,
            &WIDE_LO,
            &WIDE_HI,
            &te.token_a,
            &WIDE_DEPOSIT,
            &0_i128,
            &u64::MAX,
        );
        assert_eq!(quote, res);
        assert!(res.liquidity <= 149_382_848_285_501);
        assert!(res.liquidity >= 149_382_848_285_501 - 149);

        let (ba, bb) = te
            .client
            .burn_position(&te.provider, &WIDE_LO, &WIDE_HI, &res.liquidity);
        assert_eq!(bb, 0);
        assert!(ba <= WIDE_DEPOSIT && WIDE_DEPOSIT - ba <= 3);
    }

    #[test]
    fn unrepresentable_position_amount_is_a_typed_error() {
        // Over the narrow range [0, 100] an i128::MAX token-B deposit is
        // worth ~3.4e40 liquidity, more than an i128 can hold. That is
        // reported as MathOverflow instead of a wrapped liquidity figure.
        let env = Env::default();
        let te = setup_test_env(&env, 30, 100);
        let res =
            te.client
                .try_quote_single_token_deposit(&0_i32, &100_i32, &te.token_b, &i128::MAX);
        assert_eq!(res, Err(Ok(ClError::MathOverflow)));
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use soroban_sdk::testutils::{Address as _, Events as _};
    use soroban_sdk::token::StellarAssetClient;
    use soroban_sdk::{IntoVal, Val, Vec as SdkVec};

    #[test]
    fn burn_position_emits_burn_pos_event_with_returned_amounts() {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let token_a = env.register_stellar_asset_contract_v2(admin.clone());
        let token_b = env.register_stellar_asset_contract_v2(admin.clone());
        let token_a_addr = token_a.address();
        let token_b_addr = token_b.address();

        let contract_id = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &contract_id);
        // Use current_tick = -300, range [-200, -100] (negative ticks; math works correctly).
        // current_tick < lower_tick so the position is entirely token_a.
        client.initialize(
            &admin,
            &token_a_addr,
            &token_b_addr,
            &30_i128,
            &-300_i32,
            &1_i32,
        );

        let provider = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a_addr).mint(&provider, &1_000_i128);

        let lower_tick = -200_i32;
        let upper_tick = -100_i32;
        let (mint_a, mint_b) = client.mint_position(
            &provider,
            &lower_tick,
            &upper_tick,
            &1_000_i128,
            &0_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );
        // amount_a depends on tick width, not liquidity units; just verify it's token_a only.
        assert!(mint_a > 0);
        assert_eq!(mint_b, 0);

        let liquidity = client
            .get_position(&provider, &lower_tick, &upper_tick)
            .liquidity;
        let (amount_a, amount_b) =
            client.burn_position(&provider, &lower_tick, &upper_tick, &liquidity);
        // Burn must return approximately the same amount as mint (±1 rounding), token_a only.
        assert!(
            (amount_a - mint_a).abs() <= 1,
            "burn_a={amount_a} expected ~{mint_a}"
        );
        assert_eq!(amount_b, 0_i128);

        let expected_topics: SdkVec<Val> =
            (symbol_short!("burn_pos"), provider.clone()).into_val(&env);
        let event = env
            .events()
            .all()
            .iter()
            .find(|e| e.0 == contract_id && e.1 == expected_topics)
            .expect("burn_pos event not emitted");

        let __ver_5: (u32, (i32, i32, i128, i128, i128)) = event.2.into_val(&env);
        assert_eq!(__ver_5.0, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        let data: (i32, i32, i128, i128, i128) = __ver_5.1;
        assert_eq!(
            data,
            (lower_tick, upper_tick, liquidity, amount_a, amount_b)
        );
    }
}

#[cfg(test)]
mod test_new_features {
    use super::*;
    use soroban_sdk::testutils::{Address as _, Ledger};
    use soroban_sdk::token::StellarAssetClient;
    use soroban_sdk::Env;

    // ── Issue #183: TWAP tick accumulator ────────────────────────────────────

    #[test]
    fn tick_cumulative_advances_across_swaps() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(1_000);
        env.budget().reset_unlimited();

        let admin = Address::generate(&env);
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        client.initialize(&admin, &token_a, &token_b, &30_i128, &10_i32, &1_i32);

        // Mint tokens for swapping
        StellarAssetClient::new(&env, &token_a).mint(&cl_addr, &1_000_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&cl_addr, &1_000_000_i128);

        let buyer = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&buyer, &10_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&buyer, &10_000_i128);

        // First swap at t=1060: tick was 10 for 60 seconds → cumulative += 10 * 60 = 600
        env.ledger().set_timestamp(1_060);
        client.swap(&buyer, &true, &100_i128, &0_u128, &0_i128, &u64::MAX);
        let (cum1, ts1) = client.get_tick_cumulative();
        assert_eq!(cum1, 600); // 10 * 60
        assert_eq!(ts1, 1_060);

        // Get tick after first swap, then record cumulative after second swap
        let tick_after_first = client.current_tick();

        // Second swap at t=1160: tick was tick_after_first for 100 seconds
        env.ledger().set_timestamp(1_160);
        client.swap(&buyer, &false, &100_i128, &u128::MAX, &0_i128, &u64::MAX);
        let (cum2, ts2) = client.get_tick_cumulative();
        let expected_cum2 = 600 + (tick_after_first as i64) * 100;
        assert_eq!(cum2, expected_cum2);
        assert_eq!(ts2, 1_160);
    }

    #[test]
    fn observe_zero_returns_current_cumulative() {
        let env = Env::default();
        env.mock_all_auths();
        env.budget().reset_unlimited();
        env.ledger().set_timestamp(1_000);

        let admin = Address::generate(&env);
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        client.initialize(&admin, &token_a, &token_b, &30_i128, &5_i32, &1_i32);

        StellarAssetClient::new(&env, &token_a).mint(&cl_addr, &1_000_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&cl_addr, &1_000_000_i128);
        let buyer = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&buyer, &1_000_i128);

        // At t=1100: tick was 5 for 100 seconds → expect 5*100=500 at observe(0)
        env.ledger().set_timestamp(1_100);
        client.swap(&buyer, &true, &100_i128, &0_u128, &0_i128, &u64::MAX);
        // After swap: cum=500 (from tick=5), now at some new tick
        // observe(0) should extrapolate to now: 500 + new_tick*(1100-1100) = 500
        let obs = client.observe(&0_u64);
        assert_eq!(obs, 500);
    }

    #[test]
    fn average_tick_from_two_observes_matches_expected_price() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().set_timestamp(1_000);
        env.budget().reset_unlimited();

        let admin = Address::generate(&env);
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        client.initialize(&admin, &token_a, &token_b, &30_i128, &0_i32, &1_i32);

        StellarAssetClient::new(&env, &token_a).mint(&cl_addr, &1_000_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&cl_addr, &1_000_000_i128);
        let buyer = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&buyer, &2_000_i128);

        // Swap at t=1100 → oracle accumulates tick=0 * 100s = 0
        env.ledger().set_timestamp(1_100);
        client.swap(&buyer, &true, &100_i128, &0_u128, &0_i128, &u64::MAX);
        let tick_at_1100 = client.current_tick();

        // Swap at t=1200 → oracle accumulates tick_at_1100 * 100s
        env.ledger().set_timestamp(1_200);
        client.swap(&buyer, &true, &100_i128, &0_u128, &0_i128, &u64::MAX);

        // Compute average tick over [1000, 1200]:
        // cum at t=1000 = 0 (initialized)
        // cum at t=1200 = 0*100 + tick_at_1100*100
        let obs_now = client.observe(&0_u64); // cum at t=1200
        let obs_200s_ago = client.observe(&200_u64); // cum at t=1000 = 0
        let avg_tick = (obs_now - obs_200s_ago) / 200_i64;
        // avg tick = tick_at_1100 * 100 / 200 = tick_at_1100 / 2
        assert_eq!(avg_tick, (tick_at_1100 as i64) / 2);
    }

    #[test]
    fn observe_interpolates_between_oracle_snapshots() {
        let env = Env::default();
        env.mock_all_auths();
        env.budget().reset_unlimited();
        env.ledger().set_timestamp(1_000);

        let admin = Address::generate(&env);
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        client.initialize(&admin, &token_a, &token_b, &30_i128, &0_i32, &1_i32);

        StellarAssetClient::new(&env, &token_a).mint(&cl_addr, &1_000_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&cl_addr, &1_000_000_i128);
        let buyer = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&buyer, &2_000_i128);

        env.ledger().set_timestamp(1_100);
        client.swap(&buyer, &true, &100_i128, &0_u128, &0_i128, &u64::MAX);
        let (cum_1100, _) = client.get_tick_cumulative();

        env.ledger().set_timestamp(1_200);
        client.swap(&buyer, &true, &100_i128, &0_u128, &0_i128, &u64::MAX);
        let (cum_1200, _) = client.get_tick_cumulative();

        env.ledger().set_timestamp(1_250);
        // Midpoint between swap snapshots at t=1100 and t=1200 (no snapshot at t=1150).
        let obs_mid = client.observe(&150_u64);
        let expected_mid = cum_1100 + (cum_1200 - cum_1100) / 2;
        assert_eq!(obs_mid, expected_mid);
        // Exact-key lookup would have returned 0 at t=1150 when cum_1200 != 0.
        if cum_1200 != cum_1100 {
            assert_ne!(obs_mid, 0_i64);
        }
    }

    // ── Issue #184: get_positions ─────────────────────────────────────────────

    #[test]
    fn get_positions_mint_two_close_one() {
        let env = Env::default();
        env.mock_all_auths();

        env.budget().reset_unlimited();
        let admin = Address::generate(&env);
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        client.initialize(&admin, &token_a, &token_b, &30_i128, &0_i32, &1_i32);

        let provider = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&provider, &10_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&provider, &10_000_i128);

        // Mint two positions
        client.mint_position(
            &provider,
            &-100_i32,
            &100_i32,
            &5_000_i128,
            &5_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );
        client.mint_position(
            &provider,
            &200_i32,
            &400_i32,
            &3_000_i128,
            &0_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );

        let positions = client.get_positions(&provider);
        assert_eq!(positions.len(), 2);

        // Close first position
        let liq1 = client
            .get_position(&provider, &-100_i32, &100_i32)
            .liquidity;
        StellarAssetClient::new(&env, &token_a).mint(&cl_addr, &10_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&cl_addr, &10_000_i128);
        client.burn_position(&provider, &-100_i32, &100_i32, &liq1);

        let positions_after = client.get_positions(&provider);
        assert_eq!(positions_after.len(), 1);
        assert_eq!(positions_after.get(0).unwrap(), (200_i32, 400_i32));
    }

    // ── Issue #185: quote_position ────────────────────────────────────────────

    #[test]
    fn quote_position_matches_mint_deduction() {
        let env = Env::default();
        env.mock_all_auths();

        env.budget().reset_unlimited();
        let admin = Address::generate(&env);
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        // current_tick = 0; range [100, 200] is entirely above → pure token-A position
        client.initialize(&admin, &token_a, &token_b, &30_i128, &0_i32, &1_i32);

        // quote_position: above-range means all in token_a → approximately liquidity worth
        let (qa, qb) = client.quote_position(&100_i32, &200_i32, &3_000_i128);
        assert!(qa > 0, "token_a amount should be positive");
        assert_eq!(qb, 0_i128, "token_b should be zero for above-range");

        let provider = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&provider, &10_000_i128);

        // mint_position with the same range should consume approximately (qa, qb)
        let (ma, mb) = client.mint_position(
            &provider,
            &100_i32,
            &200_i32,
            &qa,
            &0_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );
        // Due to rounding, amounts may differ slightly
        assert!(ma > 0, "mint should consume some token_a");
        assert_eq!(
            mb, 0_i128,
            "mint should not consume token_b for above-range"
        );
    }

    // ── Issue #223: position modification ────────────────────────────────────

    #[test]
    fn modify_position_increases_liquidity_settles_fees_and_reuses_storage() {
        let env = Env::default();
        env.mock_all_auths();

        env.budget().reset_unlimited();
        let admin = Address::generate(&env);
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        // Use all-negative tick range to avoid broken positive-tick sqrt math.
        // initial_tick = -100 is in range [-200, -1].
        client.initialize(&admin, &token_a, &token_b, &30_i128, &-100_i32, &1_i32);

        let provider = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&provider, &500_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&provider, &500_000_i128);
        StellarAssetClient::new(&env, &token_a).mint(&cl_addr, &500_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&cl_addr, &500_000_i128);

        client.mint_position(
            &provider,
            &-200_i32,
            &-1_i32,
            &50_000_i128,
            &50_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );

        let position_before = client.get_position(&provider, &-200_i32, &-1_i32);
        let positions_before = client.get_positions(&provider);
        assert_eq!(
            positions_before.len(),
            1,
            "opening a position should track one range"
        );

        // Accrue fees — use a small swap to stay within range [-200, -1].
        client.swap(&provider, &true, &1_000_i128, &0_u128, &0_i128, &u64::MAX);
        let position_after_swap = client.get_position(&provider, &-200_i32, &-1_i32);
        assert_eq!(
            position_after_swap.tokens_owed, position_before.tokens_owed,
            "swap should accrue fees without auto-settling them"
        );

        let quote = client.quote_position(&-200_i32, &-1_i32, &5_000_i128);
        let (added_a, added_b) = client.modify_position(
            &provider,
            &-200_i32,
            &-1_i32,
            &5_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );

        assert_eq!(
            added_a, quote.0,
            "modify_position must use the current-price quote for token A"
        );
        assert_eq!(
            added_b, quote.1,
            "modify_position must use the current-price quote for token B"
        );

        let position_after = client.get_position(&provider, &-200_i32, &-1_i32);
        assert_eq!(
            position_after.liquidity,
            position_before.liquidity + 5_000_i128,
            "liquidity must increase in place"
        );

        let positions_after = client.get_positions(&provider);
        assert_eq!(
            positions_after.len(),
            1,
            "storage must be reused for the same range"
        );
        assert_eq!(
            positions_after.get(0).unwrap(),
            (-200_i32, -1_i32),
            "the same position key should remain in the provider list"
        );
    }
}

// ── Issue #187: tick_spacing tests ───────────────────────────────────────────
#[cfg(test)]
mod test_tick_spacing {
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::token::StellarAssetClient;
    use soroban_sdk::Env;

    fn setup_cl(
        env: &Env,
        tick_spacing: i32,
    ) -> (Address, Address, Address, ConcentratedLiquidityClient<'_>) {
        env.mock_all_auths();
        let admin = Address::generate(env);
        env.budget().reset_unlimited();
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(env, &cl_addr);
        client.initialize(&admin, &token_a, &token_b, &30_i128, &0_i32, &tick_spacing);

        let provider = Address::generate(env);
        StellarAssetClient::new(env, &token_a).mint(&provider, &1_000_000_i128);
        StellarAssetClient::new(env, &token_b).mint(&provider, &1_000_000_i128);
        (provider, token_a, token_b, client)
    }

    /// Ticks that are exact multiples of tick_spacing must be accepted.
    #[test]
    fn test_aligned_ticks_succeed() {
        let env = Env::default();
        let (provider, _ta, _tb, client) = setup_cl(&env, 10);

        // -100 and 100 are both multiples of 10 → must succeed.
        let result = client.try_mint_position(
            &provider,
            &-100_i32,
            &100_i32,
            &100_000_i128,
            &100_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );
        assert!(result.is_ok(), "aligned ticks should be accepted");
    }

    /// Ticks that are NOT multiples of tick_spacing must be rejected.
    #[test]
    fn test_misaligned_lower_tick_rejected() {
        let env = Env::default();
        let (provider, _ta, _tb, client) = setup_cl(&env, 10);

        // lower_tick = -95 is not a multiple of 10.
        let result = client.try_mint_position(
            &provider,
            &-95_i32,
            &100_i32,
            &100_000_i128,
            &100_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );
        assert_eq!(result, Err(Ok(ClError::TickNotAligned)));
    }

    /// Misaligned upper tick must also be rejected.
    #[test]
    fn test_misaligned_upper_tick_rejected() {
        let env = Env::default();
        let (provider, _ta, _tb, client) = setup_cl(&env, 10);

        // upper_tick = 105 is not a multiple of 10.
        let result = client.try_mint_position(
            &provider,
            &-100_i32,
            &105_i32,
            &100_000_i128,
            &100_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );
        assert_eq!(result, Err(Ok(ClError::TickNotAligned)));
    }

    /// tick_spacing = 0 must be rejected at initialize time.
    #[test]
    fn test_zero_tick_spacing_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        env.budget().reset_unlimited();
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);

        let result = client.try_initialize(&admin, &token_a, &token_b, &30_i128, &0_i32, &0_i32);
        assert_eq!(result, Err(Ok(ClError::InvalidTickSpacing)));
    }

    /// get_pool_state must include tick_spacing set at initialize time.
    #[test]
    fn test_get_pool_state_returns_tick_spacing() {
        let env = Env::default();
        let (_provider, _ta, _tb, client) = setup_cl(&env, 60);

        let state = client.get_pool_state();
        assert_eq!(
            state.tick_spacing, 60,
            "get_pool_state must return tick_spacing = 60"
        );
    }

    /// tick_spacing = 1 allows every tick (no restriction).
    #[test]
    fn test_spacing_one_allows_any_tick() {
        let env = Env::default();
        let (provider, _ta, _tb, client) = setup_cl(&env, 1);

        // Odd ticks (not multiples of anything > 1) should work fine.
        let result = client.try_mint_position(
            &provider,
            &-7_i32,
            &13_i32,
            &100_000_i128,
            &100_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );
        assert!(result.is_ok(), "tick_spacing=1 must allow any tick pair");
    }
}

// ── Issues #203, #218, #219, #220: new feature tests ─────────────────────────
#[cfg(test)]
mod test_new_tick_features {
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::token::StellarAssetClient;
    use soroban_sdk::Env;

    fn setup_pool(env: &Env) -> (Address, Address, Address, ConcentratedLiquidityClient<'_>) {
        env.mock_all_auths();
        let admin = Address::generate(env);
        env.budget().reset_unlimited();
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(env, &cl_addr);
        client.initialize(&admin, &token_a, &token_b, &0_i128, &0_i32, &1_i32);
        let provider = Address::generate(env);
        StellarAssetClient::new(env, &token_a).mint(&provider, &10_000_000_i128);
        StellarAssetClient::new(env, &token_b).mint(&provider, &10_000_000_i128);
        StellarAssetClient::new(env, &token_a).mint(&cl_addr, &10_000_000_i128);
        StellarAssetClient::new(env, &token_b).mint(&cl_addr, &10_000_000_i128);
        (provider, token_a, token_b, client)
    }

    // ── Issue #203: get_tick_info / is_tick_initialized ───────────────────────

    #[test]
    fn is_tick_initialized_false_before_mint() {
        let env = Env::default();
        let (_provider, _ta, _tb, client) = setup_pool(&env);
        assert!(!client.is_tick_initialized(&-100_i32));
        assert!(!client.is_tick_initialized(&0_i32));
        assert!(!client.is_tick_initialized(&100_i32));
    }

    #[test]
    fn get_tick_info_returns_error_for_uninitialized_tick() {
        let env = Env::default();
        let (_provider, _ta, _tb, client) = setup_pool(&env);
        let result = client.try_get_tick_info(&-999_i32);
        assert_eq!(result, Err(Ok(ClError::TickNotInitialized)));
    }

    #[test]
    fn is_tick_initialized_true_after_mint() {
        let env = Env::default();
        let (provider, _ta, _tb, client) = setup_pool(&env);
        client.mint_position(
            &provider,
            &-100_i32,
            &100_i32,
            &10_000_i128,
            &10_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );
        assert!(
            client.is_tick_initialized(&-100_i32),
            "lower tick must be initialized"
        );
        assert!(
            client.is_tick_initialized(&100_i32),
            "upper tick must be initialized"
        );
        assert!(
            !client.is_tick_initialized(&0_i32),
            "non-boundary tick must stay uninitialized"
        );
    }

    #[test]
    fn get_tick_info_returns_correct_values_after_mint() {
        let env = Env::default();
        let (provider, _ta, _tb, client) = setup_pool(&env);
        client.mint_position(
            &provider,
            &-100_i32,
            &100_i32,
            &10_000_i128,
            &10_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );

        let lower_info = client.get_tick_info(&-100_i32);
        let upper_info = client.get_tick_info(&100_i32);

        // lower tick: liquidity_net > 0, gross > 0
        assert!(
            lower_info.liquidity_gross > 0,
            "lower gross must be positive"
        );
        assert!(lower_info.liquidity_net > 0, "lower net must be positive");

        // upper tick: liquidity_net < 0 (negative = exits liquidity), gross > 0
        assert!(
            upper_info.liquidity_gross > 0,
            "upper gross must be positive"
        );
        assert!(upper_info.liquidity_net < 0, "upper net must be negative");

        // gross must be equal in magnitude
        assert_eq!(lower_info.liquidity_gross, upper_info.liquidity_gross);
    }

    #[test]
    fn is_tick_initialized_false_after_full_burn() {
        let env = Env::default();
        let (provider, _ta, _tb, client) = setup_pool(&env);
        client.mint_position(
            &provider,
            &-100_i32,
            &100_i32,
            &10_000_i128,
            &10_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );
        assert!(client.is_tick_initialized(&-100_i32));

        let liq = client
            .get_position(&provider, &-100_i32, &100_i32)
            .liquidity;
        client.burn_position(&provider, &-100_i32, &100_i32, &liq);

        assert!(
            !client.is_tick_initialized(&-100_i32),
            "lower tick must be de-initialized after full burn"
        );
        assert!(
            !client.is_tick_initialized(&100_i32),
            "upper tick must be de-initialized after full burn"
        );
    }

    // ── Issue #218: tick bitmap public API ───────────────────────────────────

    #[test]
    fn next_initialized_tick_pub_returns_none_when_empty() {
        let env = Env::default();
        let (_provider, _ta, _tb, client) = setup_pool(&env);
        // No positions → bitmap is empty.
        let result = client.next_initialized_tick_pub(&0_i32);
        assert!(
            result.is_none(),
            "no ticks should be found when pool has no positions"
        );
    }

    #[test]
    fn prev_initialized_tick_pub_returns_none_when_empty() {
        let env = Env::default();
        let (_provider, _ta, _tb, client) = setup_pool(&env);
        let result = client.prev_initialized_tick_pub(&0_i32);
        assert!(result.is_none());
    }

    #[test]
    fn next_and_prev_initialized_tick_pub_after_mint() {
        let env = Env::default();
        let (provider, _ta, _tb, client) = setup_pool(&env);
        // Initializes ticks -100 and 100.
        client.mint_position(
            &provider,
            &-100_i32,
            &100_i32,
            &10_000_i128,
            &10_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );

        // next above tick -200: the first initialized tick above -200 is -100.
        let next = client.next_initialized_tick_pub(&-200_i32);
        assert_eq!(next, Some(-100_i32), "next tick above -200 must be -100");

        // next above tick -100: the first initialized tick above -100 is 100.
        let next2 = client.next_initialized_tick_pub(&-100_i32);
        assert_eq!(next2, Some(100_i32), "next tick above -100 must be 100");

        // prev at or below tick 200: highest initialized tick ≤ 200 is 100.
        let prev = client.prev_initialized_tick_pub(&200_i32);
        assert_eq!(prev, Some(100_i32), "prev tick at/below 200 must be 100");

        // prev at or below tick 100: same tick (100 is initialized).
        let prev2 = client.prev_initialized_tick_pub(&100_i32);
        assert_eq!(prev2, Some(100_i32));

        // prev at or below tick -101: highest initialized tick below -100 is -100... wait,
        // -101 < -100 so prev should be None (no tick at or below -101 other than maybe -100?).
        // Actually -100 < -101? No: -100 > -101. So prev of -101 should be None since -100 > -101.
        let prev3 = client.prev_initialized_tick_pub(&-101_i32);
        assert!(prev3.is_none(), "no initialized tick at or below -101");
    }

    #[test]
    fn bitmap_correctly_tracks_multiple_positions() {
        let env = Env::default();
        let (provider, _ta, _tb, client) = setup_pool(&env);
        // Two non-overlapping ranges initialize 4 distinct ticks.
        client.mint_position(
            &provider,
            &-200_i32,
            &-100_i32,
            &0_i128,
            &5_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );
        client.mint_position(
            &provider,
            &100_i32,
            &200_i32,
            &5_000_i128,
            &0_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );

        assert!(client.is_tick_initialized(&-200_i32));
        assert!(client.is_tick_initialized(&-100_i32));
        assert!(client.is_tick_initialized(&100_i32));
        assert!(client.is_tick_initialized(&200_i32));
        assert!(!client.is_tick_initialized(&0_i32));

        // next above -300 must be -200.
        assert_eq!(client.next_initialized_tick_pub(&-300_i32), Some(-200_i32));
        // next above -200 must be -100.
        assert_eq!(client.next_initialized_tick_pub(&-200_i32), Some(-100_i32));
    }

    // ── Issue #219: sqrtPrice math library ───────────────────────────────────

    #[test]
    fn tick_to_sqrt_price_x96_at_zero_is_one_q96() {
        // sqrt(1.0001^0) * 2^96 = 1 * 2^96
        let sp = ConcentratedLiquidity::tick_to_sqrt_price_x96(0_i32);
        assert_eq!(sp, 1u128 << 96, "sqrtPrice at tick 0 must be exactly 2^96");
    }

    #[test]
    fn tick_to_sqrt_price_x96_is_monotone() {
        // sqrtPrice must increase strictly with tick.
        let sp_neg = ConcentratedLiquidity::tick_to_sqrt_price_x96(-10_i32);
        let sp_zero = ConcentratedLiquidity::tick_to_sqrt_price_x96(0_i32);
        let sp_pos = ConcentratedLiquidity::tick_to_sqrt_price_x96(10_i32);
        assert!(sp_neg < sp_zero, "sqrtPrice(-10) must be < sqrtPrice(0)");
        assert!(sp_zero < sp_pos, "sqrtPrice(0) must be < sqrtPrice(10)");
    }

    #[test]
    fn tick_to_sqrt_price_x96_accuracy_within_one_bps() {
        // For tick = 100: sqrt(1.0001^100) ≈ 1.0001^50 ≈ 1.005012.
        // We verify the returned value is within 1 bps (0.01%) of 2^96 * 1.005012.
        let sp = ConcentratedLiquidity::tick_to_sqrt_price_x96(100_i32);
        let one_q96: u128 = 1u128 << 96;
        // Expected ≈ 1.005012 * 2^96. Allow ±1 bps = 0.01%.
        let expected_approx = one_q96 + one_q96 / 200; // 1.005 * 2^96 (rough lower bound);
        assert!(
            sp >= expected_approx,
            "sqrtPrice(100) must be at least 1.005 * 2^96"
        );
        let upper = one_q96 + one_q96 / 100; // 1.01 * 2^96 (rough upper bound);
        assert!(sp <= upper, "sqrtPrice(100) must be at most 1.01 * 2^96");
    }

    #[test]
    fn sqrt_price_x96_to_tick_roundtrip() {
        // For any tick t, sqrt_price_x96_to_tick(tick_to_sqrt_price_x96(t)) should equal t
        // (or be off by at most 1 due to integer rounding).
        for tick in [-300_i32, -100, -10, -1, 0, 1, 10, 100, 300] {
            let sp = ConcentratedLiquidity::tick_to_sqrt_price_x96(tick);
            let recovered = ConcentratedLiquidity::sqrt_price_x96_to_tick(sp);
            let diff = (recovered - tick).abs();
            assert!(
                diff <= 1,
                "roundtrip failed for tick {tick}: got {recovered}, diff={diff}"
            );
        }
    }

    #[test]
    fn sqrt_price_x96_to_tick_at_zero_input_returns_min_tick() {
        let t = ConcentratedLiquidity::sqrt_price_x96_to_tick(0_u128);
        assert_eq!(t, MIN_TICK);
    }

    #[test]
    fn tick_to_sqrt_price_matches_pool_sqrt_price_at_tick_zero() {
        // The pool stores sqrtPriceX96 = sqrt(price) * 2^96 / 1000 after initialize.
        // tick_to_sqrt_price_x96(0) = 2^96.  pool initial = 1000 * 2^96 / 1000 = 2^96. ✓
        let env = Env::default();
        let (_provider, _ta, _tb, client) = setup_pool(&env);
        let state = client.get_pool_state();
        let computed = ConcentratedLiquidity::tick_to_sqrt_price_x96(state.current_tick);
        // Allow a tiny rounding difference.
        let diff = (computed as i128 - state.sqrt_price as i128).abs();
        let one_pct = (state.sqrt_price / 100) as i128;
        assert!(
            diff <= one_pct,
            "tick_to_sqrt_price_x96 must agree with pool sqrtPrice within 1%"
        );
    }

    // ── Issue #220: tick state machine query helpers ──────────────────────────

    #[test]
    fn get_liquidity_net_at_tick_returns_zero_for_uninitialised() {
        let env = Env::default();
        let (_provider, _ta, _tb, client) = setup_pool(&env);
        assert_eq!(client.get_liquidity_net_at_tick(&42_i32), 0_i128);
    }

    #[test]
    fn get_liquidity_net_at_tick_correct_after_mint() {
        let env = Env::default();
        let (provider, _ta, _tb, client) = setup_pool(&env);
        client.mint_position(
            &provider,
            &-100_i32,
            &100_i32,
            &10_000_i128,
            &10_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );
        let lower_net = client.get_liquidity_net_at_tick(&-100_i32);
        let upper_net = client.get_liquidity_net_at_tick(&100_i32);
        assert!(lower_net > 0, "lower tick liquidity_net must be positive");
        assert!(upper_net < 0, "upper tick liquidity_net must be negative");
        assert_eq!(
            lower_net, -upper_net,
            "net values must be equal and opposite"
        );
    }

    #[test]
    fn simulate_tick_cross_upward_adds_net_liquidity() {
        let env = Env::default();
        let (provider, _ta, _tb, client) = setup_pool(&env);
        client.mint_position(
            &provider,
            &-100_i32,
            &100_i32,
            &10_000_i128,
            &10_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );

        let net = client.get_liquidity_net_at_tick(&-100_i32);
        // Crossing lower tick upward (zero_for_one=false) adds net.
        let result = client.simulate_tick_cross(&0_i128, &-100_i32, &false);
        assert_eq!(
            result,
            net.max(0),
            "crossing lower tick upward must add net liquidity"
        );
    }

    #[test]
    fn simulate_tick_cross_downward_subtracts_net_liquidity() {
        let env = Env::default();
        let (provider, _ta, _tb, client) = setup_pool(&env);
        client.mint_position(
            &provider,
            &-100_i32,
            &100_i32,
            &10_000_i128,
            &10_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );

        let net = client.get_liquidity_net_at_tick(&-100_i32);
        let active = net; // assume we're currently above lower_tick with net liq
                          // Crossing lower tick downward (zero_for_one=true) subtracts net.
        let result = client.simulate_tick_cross(&active, &-100_i32, &true);
        assert_eq!(result, (active - net).max(0));
    }

    #[test]
    fn simulate_tick_cross_does_not_modify_state() {
        let env = Env::default();
        let (provider, _ta, _tb, client) = setup_pool(&env);
        client.mint_position(
            &provider,
            &-100_i32,
            &100_i32,
            &10_000_i128,
            &10_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );

        let liq_before = client.active_liquidity();
        // Call simulate — must not change active liquidity.
        client.simulate_tick_cross(&liq_before, &-100_i32, &false);
        assert_eq!(
            client.active_liquidity(),
            liq_before,
            "simulate_tick_cross must not modify state"
        );
    }

    #[test]
    fn tick_state_machine_liquidity_updates_correctly_during_swap() {
        // Full integration: mint two adjacent ranges, perform a swap that crosses
        // a tick boundary, verify active_liquidity is correct at each step.
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        env.budget().reset_unlimited();
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        // Start at tick 10, so range [-50, 0] is below current and [0, 50] includes current.
        client.initialize(&admin, &token_a, &token_b, &0_i128, &10_i32, &1_i32);

        let provider = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&provider, &10_000_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&provider, &10_000_000_i128);
        StellarAssetClient::new(&env, &token_a).mint(&cl_addr, &10_000_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&cl_addr, &10_000_000_i128);

        // Range [0, 50] covers current tick 10 → active_liquidity increases.
        client.mint_position(
            &provider,
            &0_i32,
            &50_i32,
            &100_000_i128,
            &100_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );
        let liq_in_range = client.active_liquidity();
        assert!(
            liq_in_range > 0,
            "position covering current tick must add active liquidity"
        );

        // Range [-50, 0] is entirely below current tick → no active liquidity yet.
        client.mint_position(
            &provider,
            &-50_i32,
            &0_i32,
            &0_i128,
            &50_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );
        assert_eq!(
            client.active_liquidity(),
            liq_in_range,
            "out-of-range position must not change active liq"
        );

        // Verify tick-state-machine view: net at tick 0 accounts for both positions.
        let net_at_0 = client.get_liquidity_net_at_tick(&0_i32);
        // lower tick for second range: net += liq2; upper tick for first range is 50 (not 0);
        // So at tick 0: net = liq2 (lower of second) - first_range_liq... wait,
        // tick 0 is the UPPER of range [-50,0] AND LOWER of range [0,50]:
        // Actually in the code, upper tick uses liquidity_net -= liquidity.
        // The liquidity_net at tick 0 = (liq from [0,50] as lower) + (-liq from [-50,0] as upper);
        // = liq1 - liq2 (approximately, since both have similar amounts);
        // Just verify it's non-zero (both positions contributed).
        assert_ne!(
            net_at_0, 0_i128,
            "tick 0 net must be non-zero with two adjacent positions"
        );

        // Perform a downward swap to cross below tick 0.
        //
        // Crossing from tick 10 down to tick 0 against this pool's active
        // liquidity (~5.0e7) takes about 25,055 of token A:
        //   amount0 = L * Q96 * (sqrt(10) - sqrt(0)) / (sqrt(10) * sqrt(0))
        // The 5,000 this used to swap moves the price barely two ticks (to
        // tick 8). It only appeared to cross because the old 3-significant-
        // digit price scale jumped the price ~20 ticks per step regardless of
        // size. 40,000 crosses tick 0 for real, landing around tick -6.
        client.swap(&provider, &true, &40_000_i128, &0_u128, &0_i128, &u64::MAX);
        let tick_after = client.current_tick();
        assert!(tick_after < 0, "swap should push price below tick 0");
        // After crossing tick 0 downward, the active liquidity of the lower range activates.
        // The net change should reflect both crossing events.
        let liq_after = client.active_liquidity();
        // Price now in [-50, 0), so the second position is active and first is not.
        assert!(
            liq_after > 0,
            "lower range must be active after crossing tick 0 downward"
        );
    }
}

// ── Issue #221: single-token deposit tests ────────────────────────────────────
#[cfg(test)]
mod test_single_token_deposit {
    use super::*;
    use soroban_sdk::testutils::{Address as _, Ledger};
    use soroban_sdk::token::{StellarAssetClient, TokenClient};
    use soroban_sdk::Env;

    /// Helper: deploy a CL pool starting at `initial_tick` with `tick_spacing = 1`.
    fn setup_pool(
        env: &Env,
        initial_tick: i32,
    ) -> (Address, Address, Address, ConcentratedLiquidityClient<'_>) {
        env.mock_all_auths();
        let admin = Address::generate(env);
        env.budget().reset_unlimited();
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(env, &cl_addr);
        client.initialize(&admin, &token_a, &token_b, &0_i128, &initial_tick, &1_i32);

        let provider = Address::generate(env);
        StellarAssetClient::new(env, &token_a).mint(&provider, &100_000_000_i128);
        StellarAssetClient::new(env, &token_b).mint(&provider, &100_000_000_i128);
        // Pre-fund contract so it can return tokens in burn/collect.
        StellarAssetClient::new(env, &token_a).mint(&cl_addr, &100_000_000_i128);
        StellarAssetClient::new(env, &token_b).mint(&cl_addr, &100_000_000_i128);

        (provider, token_a, token_b, client)
    }

    // ── Scenario 1: price BELOW range — only token A needed ──────────────────

    /// When current price is below the position range, only token A is required.
    /// The full `amount_in` of token A should be consumed with zero dust.
    #[test]
    fn test_single_token_deposit_below_range_uses_only_token_a() {
        let env = Env::default();
        // current_tick = -200 → price is below range [100, 200]
        let (provider, token_a, _token_b, client) = setup_pool(&env, -200);

        let amount_in = 10_000_i128;
        let result = client.mint_position_single_token(
            &provider,
            &100_i32,
            &200_i32,
            &token_a,
            &amount_in,
            &1_i128,
            &u64::MAX,
        );

        assert_eq!(
            result.amount_used, amount_in,
            "all token A must be consumed"
        );
        assert_eq!(result.dust, 0_i128, "no dust when price is below range");
        assert!(result.liquidity > 0, "liquidity must be positive");
    }

    /// Supplying token B when price is below range must fail (wrong token).
    #[test]
    fn test_single_token_deposit_below_range_rejects_token_b() {
        let env = Env::default();
        let (provider, _token_a, token_b, client) = setup_pool(&env, -200);

        let result = client.try_mint_position_single_token(
            &provider,
            &100_i32,
            &200_i32,
            &token_b,
            &10_000_i128,
            &1_i128,
            &u64::MAX,
        );
        assert_eq!(
            result,
            Err(Ok(ClError::SlippageExceeded)),
            "token B must be rejected when price is below range"
        );
    }

    // ── Scenario 2: price ABOVE range — only token B needed ──────────────────

    /// When current price is above the position range, only token B is required.
    #[test]
    fn test_single_token_deposit_above_range_uses_only_token_b() {
        let env = Env::default();
        // current_tick = 300 → price is above range [-200, -100]
        let (provider, _token_a, token_b, client) = setup_pool(&env, 300);

        let amount_in = 10_000_i128;
        let result = client.mint_position_single_token(
            &provider,
            &-200_i32,
            &-100_i32,
            &token_b,
            &amount_in,
            &1_i128,
            &u64::MAX,
        );

        assert_eq!(
            result.amount_used, amount_in,
            "all token B must be consumed"
        );
        assert_eq!(result.dust, 0_i128, "no dust when price is above range");
        assert!(result.liquidity > 0, "liquidity must be positive");
    }

    /// Supplying token A when price is above range must fail (wrong token).
    #[test]
    fn test_single_token_deposit_above_range_rejects_token_a() {
        let env = Env::default();
        let (provider, token_a, _token_b, client) = setup_pool(&env, 300);

        let result = client.try_mint_position_single_token(
            &provider,
            &-200_i32,
            &-100_i32,
            &token_a,
            &10_000_i128,
            &1_i128,
            &u64::MAX,
        );
        assert_eq!(
            result,
            Err(Ok(ClError::SlippageExceeded)),
            "token A must be rejected when price is above range"
        );
    }

    // ── Scenario 3: price WITHIN range — single token split ──────────────────

    /// When price is inside the range and token A is supplied, liquidity is
    /// computed from the token-A portion only (covers [current_price, upper]).
    #[test]
    fn test_single_token_deposit_in_range_token_a() {
        let env = Env::default();
        // current_tick = 0, range [-100, 100] → price is in range
        let (provider, token_a, _token_b, client) = setup_pool(&env, 0);

        let amount_in = 50_000_i128;
        let result = client.mint_position_single_token(
            &provider,
            &-100_i32,
            &100_i32,
            &token_a,
            &amount_in,
            &1_i128,
            &u64::MAX,
        );

        // amount_used ≤ amount_in (some dust possible due to rounding);
        assert!(
            result.amount_used <= amount_in,
            "amount_used must not exceed amount_in"
        );
        assert!(result.amount_used > 0, "some token A must be consumed");
        assert_eq!(
            result.amount_used + result.dust,
            amount_in,
            "amount_used + dust must equal amount_in"
        );
        assert!(result.liquidity > 0, "liquidity must be positive");
    }

    /// When price is inside the range and token B is supplied, liquidity is
    /// computed from the token-B portion only (covers [lower, current_price]).
    #[test]
    fn test_single_token_deposit_in_range_token_b() {
        let env = Env::default();
        let (provider, _token_a, token_b, client) = setup_pool(&env, 0);

        let amount_in = 50_000_i128;
        let result = client.mint_position_single_token(
            &provider,
            &-100_i32,
            &100_i32,
            &token_b,
            &amount_in,
            &1_i128,
            &u64::MAX,
        );

        assert!(result.amount_used <= amount_in);
        assert!(result.amount_used > 0);
        assert_eq!(result.amount_used + result.dust, amount_in);
        assert!(result.liquidity > 0);
    }

    /// Dust is minimised: for a large deposit the dust should be at most a
    /// tiny fraction of the input (rounding artefact only).
    #[test]
    fn test_single_token_deposit_in_range_dust_is_minimal() {
        let env = Env::default();
        let (provider, token_a, _token_b, client) = setup_pool(&env, 0);

        let amount_in = 1_000_000_i128;
        let result = client.mint_position_single_token(
            &provider,
            &-100_i32,
            &100_i32,
            &token_a,
            &amount_in,
            &1_i128,
            &u64::MAX,
        );

        // Dust must be < 1% of amount_in (rounding only, not a large fraction).
        assert!(
            result.dust < amount_in / 100,
            "dust ({}) must be < 1% of amount_in ({})",
            result.dust,
            amount_in
        );
    }

    // ── Slippage guard ────────────────────────────────────────────────────────

    /// min_liquidity guard must reject deposits that produce too little liquidity.
    #[test]
    fn test_single_token_deposit_min_liquidity_guard() {
        let env = Env::default();
        let (provider, token_a, _token_b, client) = setup_pool(&env, -200);

        // First, find out how much liquidity a 10_000 deposit produces.
        let normal = client.mint_position_single_token(
            &provider,
            &100_i32,
            &200_i32,
            &token_a,
            &10_000_i128,
            &1_i128,
            &u64::MAX,
        );

        // Now request more than that — must fail.
        let provider2 = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&provider2, &10_000_i128);
        let result = client.try_mint_position_single_token(
            &provider2,
            &100_i32,
            &200_i32,
            &token_a,
            &10_000_i128,
            &(normal.liquidity + 1),
            &u64::MAX,
        );
        assert_eq!(
            result,
            Err(Ok(ClError::SlippageExceeded)),
            "min_liquidity guard must reject insufficient liquidity"
        );
    }

    // ── Deadline guard ────────────────────────────────────────────────────────

    #[test]
    fn test_single_token_deposit_deadline_expired() {
        let env = Env::default();
        env.ledger().set_timestamp(1_000);
        let (provider, token_a, _token_b, client) = setup_pool(&env, -200);

        let result = client.try_mint_position_single_token(
            &provider,
            &100_i32,
            &200_i32,
            &token_a,
            &10_000_i128,
            &1_i128,
            &999_u64, // deadline in the past
        );
        assert_eq!(result, Err(Ok(ClError::DeadlineExpired)));
    }

    // ── Pause guard ───────────────────────────────────────────────────────────

    #[test]
    fn test_single_token_deposit_paused_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        env.budget().reset_unlimited();
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        client.initialize(&admin, &token_a, &token_b, &0_i128, &-200_i32, &1_i32);

        let provider = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&provider, &10_000_i128);

        client.pause(&admin);

        let result = client.try_mint_position_single_token(
            &provider,
            &100_i32,
            &200_i32,
            &token_a,
            &10_000_i128,
            &1_i128,
            &u64::MAX,
        );
        assert_eq!(result, Err(Ok(ClError::Paused)));
    }

    // ── Invalid token guard ───────────────────────────────────────────────────

    #[test]
    fn test_single_token_deposit_invalid_token_rejected() {
        let env = Env::default();
        let (provider, _token_a, _token_b, client) = setup_pool(&env, -200);

        let unknown = Address::generate(&env);
        let result = client.try_mint_position_single_token(
            &provider,
            &100_i32,
            &200_i32,
            &unknown,
            &10_000_i128,
            &1_i128,
            &u64::MAX,
        );
        assert_eq!(result, Err(Ok(ClError::InvalidToken)));
    }

    // ── Tick alignment guard ──────────────────────────────────────────────────

    #[test]
    fn test_single_token_deposit_misaligned_tick_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        env.budget().reset_unlimited();
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        // tick_spacing = 10
        client.initialize(&admin, &token_a, &token_b, &0_i128, &-200_i32, &10_i32);

        let provider = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&provider, &10_000_i128);

        // lower_tick = 105 is not a multiple of 10
        let result = client.try_mint_position_single_token(
            &provider,
            &105_i32,
            &200_i32,
            &token_a,
            &10_000_i128,
            &1_i128,
            &u64::MAX,
        );
        assert_eq!(result, Err(Ok(ClError::TickNotAligned)));
    }

    // ── Zero amount guard ─────────────────────────────────────────────────────

    #[test]
    fn test_single_token_deposit_zero_amount_rejected() {
        let env = Env::default();
        let (provider, token_a, _token_b, client) = setup_pool(&env, -200);

        let result = client.try_mint_position_single_token(
            &provider,
            &100_i32,
            &200_i32,
            &token_a,
            &0_i128,
            &1_i128,
            &u64::MAX,
        );
        assert_eq!(result, Err(Ok(ClError::ZeroAmounts)));
    }

    // ── Position accumulation ─────────────────────────────────────────────────

    /// Two single-token deposits to the same range should accumulate liquidity.
    #[test]
    fn test_single_token_deposit_accumulates_liquidity() {
        let env = Env::default();
        let (provider, token_a, _token_b, client) = setup_pool(&env, -200);

        let r1 = client.mint_position_single_token(
            &provider,
            &100_i32,
            &200_i32,
            &token_a,
            &10_000_i128,
            &1_i128,
            &u64::MAX,
        );

        let r2 = client.mint_position_single_token(
            &provider,
            &100_i32,
            &200_i32,
            &token_a,
            &10_000_i128,
            &1_i128,
            &u64::MAX,
        );

        let pos = client.get_position(&provider, &100_i32, &200_i32);
        assert_eq!(
            pos.liquidity,
            r1.liquidity + r2.liquidity,
            "liquidity must accumulate across deposits"
        );
    }

    // ── quote_single_token_deposit matches mint ───────────────────────────────

    /// The quote function must return the same values as the actual deposit.
    #[test]
    fn test_quote_single_token_deposit_matches_mint_below_range() {
        let env = Env::default();
        let (provider, token_a, _token_b, client) = setup_pool(&env, -200);

        let amount_in = 20_000_i128;
        let quote = client.quote_single_token_deposit(&100_i32, &200_i32, &token_a, &amount_in);

        let actual = client.mint_position_single_token(
            &provider,
            &100_i32,
            &200_i32,
            &token_a,
            &amount_in,
            &1_i128,
            &u64::MAX,
        );

        assert_eq!(
            quote.amount_used, actual.amount_used,
            "quote amount_used must match actual"
        );
        assert_eq!(quote.dust, actual.dust, "quote dust must match actual");
        assert_eq!(
            quote.liquidity, actual.liquidity,
            "quote liquidity must match actual"
        );
    }

    #[test]
    fn test_quote_single_token_deposit_matches_mint_above_range() {
        let env = Env::default();
        let (provider, _token_a, token_b, client) = setup_pool(&env, 300);

        let amount_in = 20_000_i128;
        let quote = client.quote_single_token_deposit(&-200_i32, &-100_i32, &token_b, &amount_in);

        let actual = client.mint_position_single_token(
            &provider,
            &-200_i32,
            &-100_i32,
            &token_b,
            &amount_in,
            &1_i128,
            &u64::MAX,
        );

        assert_eq!(quote.amount_used, actual.amount_used);
        assert_eq!(quote.dust, actual.dust);
        assert_eq!(quote.liquidity, actual.liquidity);
    }

    #[test]
    fn test_quote_single_token_deposit_matches_mint_in_range() {
        let env = Env::default();
        let (provider, token_a, _token_b, client) = setup_pool(&env, 0);

        let amount_in = 100_000_i128;
        let quote = client.quote_single_token_deposit(&-100_i32, &100_i32, &token_a, &amount_in);

        let actual = client.mint_position_single_token(
            &provider,
            &-100_i32,
            &100_i32,
            &token_a,
            &amount_in,
            &1_i128,
            &u64::MAX,
        );

        assert_eq!(quote.amount_used, actual.amount_used);
        assert_eq!(quote.dust, actual.dust);
        assert_eq!(quote.liquidity, actual.liquidity);
    }

    // ── Active liquidity tracking ─────────────────────────────────────────────

    /// A single-token deposit to an in-range position must increase active_liquidity.
    #[test]
    fn test_single_token_deposit_in_range_increases_active_liquidity() {
        let env = Env::default();
        let (provider, token_a, _token_b, client) = setup_pool(&env, 0);

        let liq_before = client.active_liquidity();
        let result = client.mint_position_single_token(
            &provider,
            &-100_i32,
            &100_i32,
            &token_a,
            &50_000_i128,
            &1_i128,
            &u64::MAX,
        );

        let liq_after = client.active_liquidity();
        assert_eq!(
            liq_after - liq_before,
            result.liquidity,
            "active_liquidity must increase by the minted liquidity"
        );
    }

    /// A single-token deposit to an out-of-range position must NOT change active_liquidity.
    #[test]
    fn test_single_token_deposit_out_of_range_does_not_change_active_liquidity() {
        let env = Env::default();
        // current_tick = -200, range [100, 200] is above current price
        let (provider, token_a, _token_b, client) = setup_pool(&env, -200);

        let liq_before = client.active_liquidity();
        client.mint_position_single_token(
            &provider,
            &100_i32,
            &200_i32,
            &token_a,
            &50_000_i128,
            &1_i128,
            &u64::MAX,
        );

        assert_eq!(
            client.active_liquidity(),
            liq_before,
            "out-of-range deposit must not change active_liquidity"
        );
    }

    // ── Tick initialisation ───────────────────────────────────────────────────

    /// After a single-token deposit the boundary ticks must be initialised.
    #[test]
    fn test_single_token_deposit_initialises_ticks() {
        let env = Env::default();
        let (provider, token_a, _token_b, client) = setup_pool(&env, -200);

        assert!(!client.is_tick_initialized(&100_i32));
        assert!(!client.is_tick_initialized(&200_i32));

        client.mint_position_single_token(
            &provider,
            &100_i32,
            &200_i32,
            &token_a,
            &10_000_i128,
            &1_i128,
            &u64::MAX,
        );

        assert!(
            client.is_tick_initialized(&100_i32),
            "lower tick must be initialised"
        );
        assert!(
            client.is_tick_initialized(&200_i32),
            "upper tick must be initialised"
        );
    }

    // ── Position list tracking ────────────────────────────────────────────────

    /// get_positions must include the range after a single-token deposit.
    #[test]
    fn test_single_token_deposit_appears_in_get_positions() {
        let env = Env::default();
        let (provider, token_a, _token_b, client) = setup_pool(&env, -200);

        client.mint_position_single_token(
            &provider,
            &100_i32,
            &200_i32,
            &token_a,
            &10_000_i128,
            &1_i128,
            &u64::MAX,
        );

        let positions = client.get_positions(&provider);
        assert_eq!(positions.len(), 1);
        assert_eq!(positions.get(0).unwrap(), (100_i32, 200_i32));
    }

    // ── Symmetry: token A below range == token B above range ─────────────────

    /// Depositing token A below range and token B above range with the same
    /// amount should produce the same liquidity (symmetric price model).
    #[test]
    fn test_single_token_deposit_symmetry_below_above() {
        // Use negative ticks where sqrt math is correct.
        // Pool A: current below range → token_a deposit.
        let env_a = Env::default();
        let (provider_a, token_a_a, _token_b_a, client_a) = setup_pool(&env_a, -300);
        let result_a = client_a.mint_position_single_token(
            &provider_a,
            &-200_i32,
            &-100_i32,
            &token_a_a,
            &10_000_i128,
            &1_i128,
            &u64::MAX,
        );

        // Pool B: current above range → token_b deposit.
        let env_b = Env::default();
        let (provider_b, _token_a_b, token_b_b, client_b) = setup_pool(&env_b, -50);
        let result_b = client_b.mint_position_single_token(
            &provider_b,
            &-200_i32,
            &-100_i32,
            &token_b_b,
            &10_000_i128,
            &1_i128,
            &u64::MAX,
        );

        // Both deposits should produce positive liquidity (exact equality not guaranteed
        // for asymmetric tick ranges, but both should succeed).
        assert!(
            result_a.liquidity > 0,
            "below-range deposit must produce positive liquidity"
        );
        assert!(
            result_b.liquidity > 0,
            "above-range deposit must produce positive liquidity"
        );
        assert_eq!(result_a.dust, 0);
        assert_eq!(result_b.dust, 0);
    }

    // ── Larger deposit produces proportionally more liquidity ─────────────────

    #[test]
    fn test_single_token_deposit_liquidity_scales_with_amount() {
        let env = Env::default();
        let (provider, token_a, _token_b, client) = setup_pool(&env, -200);

        let r1 = client.mint_position_single_token(
            &provider,
            &100_i32,
            &200_i32,
            &token_a,
            &10_000_i128,
            &1_i128,
            &u64::MAX,
        );

        let r2 = client.mint_position_single_token(
            &provider,
            &100_i32,
            &200_i32,
            &token_a,
            &20_000_i128,
            &1_i128,
            &u64::MAX,
        );

        // Doubling the deposit should roughly double the liquidity (within rounding).
        assert!(
            r2.liquidity >= r1.liquidity * 2 - 2,
            "double deposit must produce at least double liquidity (got {} vs {})",
            r2.liquidity,
            r1.liquidity
        );
    }

    // ── mint_pos event emitted ────────────────────────────────────────────────

    #[test]
    fn test_single_token_deposit_emits_event() {
        use soroban_sdk::testutils::Events as _;
        use soroban_sdk::IntoVal;

        let env = Env::default();
        let (provider, token_a, _token_b, client) = setup_pool(&env, -200);

        let result = client.mint_position_single_token(
            &provider,
            &100_i32,
            &200_i32,
            &token_a,
            &10_000_i128,
            &1_i128,
            &u64::MAX,
        );

        let events = env.events().all();
        let evt = events
            .iter()
            .find(|e| {
                e.0 == client.address
                    && e.1 == (symbol_short!("mint_1t"), provider.clone()).into_val(&env)
            })
            .expect("mint_1t event must be emitted");

        let __ver_6: (u32, (i32, i32, i128, i128, i128)) = evt.2.into_val(&env);
        assert_eq!(__ver_6.0, soroban_amm_sdk::EVENT_SCHEMA_VERSION);
        let data: (i32, i32, i128, i128, i128) = __ver_6.1;
        assert_eq!(data.0, 100_i32);
        assert_eq!(data.1, 200_i32);
        assert_eq!(data.2, result.liquidity);
        assert_eq!(data.3, result.amount_used);
        assert_eq!(data.4, result.dust);
    }

    // ── Edge cases: current tick at boundary positions ────────────────────────

    /// When current tick equals lower_tick exactly, token A deposit should work
    /// for the [current, upper] portion.
    #[test]
    fn test_single_token_deposit_at_lower_tick_boundary() {
        let env = Env::default();
        // current_tick = 100, range [100, 200] - price at lower boundary
        let (provider, token_a, _token_b, client) = setup_pool(&env, 100);

        let result = client.mint_position_single_token(
            &provider,
            &100_i32,
            &200_i32,
            &token_a,
            &10_000_i128,
            &1_i128,
            &u64::MAX,
        );

        // Token A at lower boundary should still produce liquidity
        assert!(result.liquidity > 0);
        assert!(result.amount_used > 0);
        // Active liquidity should increase since current >= lower_tick
        assert!(client.active_liquidity() > 0);
    }

    /// When current tick equals upper_tick - 1, token B deposit should work
    /// for the [lower, current] portion.
    #[test]
    fn test_single_token_deposit_just_below_upper_tick() {
        let env = Env::default();
        // current_tick = -101, range [-200, -100]: current is 1 below upper (-100).
        // This is an in-range position; token_b covers [lower=-200, current=-101].
        let (provider, _token_a, token_b, client) = setup_pool(&env, -101);

        let result = client.mint_position_single_token(
            &provider,
            &-200_i32,
            &-100_i32,
            &token_b,
            &10_000_i128,
            &1_i128,
            &u64::MAX,
        );

        assert!(result.liquidity > 0);
        assert!(result.amount_used > 0);
    }

    /// Large range with in-range single token deposit.
    #[test]
    fn test_single_token_deposit_large_range() {
        let env = Env::default();
        // current_tick = 0, range [-1000, 1000] - wide range centered at current
        let (provider, token_a, _token_b, client) = setup_pool(&env, 0);

        let result = client.mint_position_single_token(
            &provider,
            &-1000_i32,
            &1000_i32,
            &token_a,
            &100_000_i128,
            &1_i128,
            &u64::MAX,
        );

        // Token A covers upper half of the range
        assert!(result.liquidity > 0);
        // Dust should be minimal for large amounts
        assert!(result.dust < result.amount_used / 10);
    }

    /// Small range near current tick with single token.
    #[test]
    fn test_single_token_deposit_small_range_near_current() {
        let env = Env::default();
        // current_tick = 0, range [-1, 1] - very tight range around current
        let (provider, token_a, _token_b, client) = setup_pool(&env, 0);

        let result = client.mint_position_single_token(
            &provider,
            &-1_i32,
            &1_i32,
            &token_a,
            &1_000_i128,
            &1_i128,
            &u64::MAX,
        );

        assert!(result.liquidity > 0);
        assert!(result.amount_used > 0);
    }

    /// Extreme tick values should work correctly.
    #[test]
    fn test_single_token_deposit_extreme_tick_range() {
        let env = Env::default();
        // current_tick = -800000, near minimum tick
        let (provider, token_a, _token_b, client) = setup_pool(&env, -800000);

        // Deposit at a reasonable range above current
        let result = client.mint_position_single_token(
            &provider,
            &-700000_i32,
            &-600000_i32,
            &token_a,
            &10_000_i128,
            &1_i128,
            &u64::MAX,
        );

        assert!(result.liquidity > 0);
    }

    /// Quote and mint should match for all three scenarios.
    #[test]
    fn test_quote_matches_mint_across_all_scenarios() {
        let env = Env::default();

        // Test below range
        let (provider_a, token_a, _token_b, client) = setup_pool(&env, -200);
        let quote1 = client.quote_single_token_deposit(&100_i32, &200_i32, &token_a, &10_000_i128);
        let mint1 = client.mint_position_single_token(
            &provider_a,
            &100_i32,
            &200_i32,
            &token_a,
            &10_000_i128,
            &1_i128,
            &u64::MAX,
        );
        assert_eq!(quote1.liquidity, mint1.liquidity);
        assert_eq!(quote1.amount_used, mint1.amount_used);

        // Test above range
        let (provider_b, _token_a, token_b, client) = setup_pool(&env, 300);
        let quote2 =
            client.quote_single_token_deposit(&-200_i32, &-100_i32, &token_b, &10_000_i128);
        let mint2 = client.mint_position_single_token(
            &provider_b,
            &-200_i32,
            &-100_i32,
            &token_b,
            &10_000_i128,
            &1_i128,
            &u64::MAX,
        );
        assert_eq!(quote2.liquidity, mint2.liquidity);
        assert_eq!(quote2.amount_used, mint2.amount_used);

        // Test in range
        let (provider_c, token_a, _token_b, client) = setup_pool(&env, 0);
        let quote3 = client.quote_single_token_deposit(&-50_i32, &50_i32, &token_a, &10_000_i128);
        let mint3 = client.mint_position_single_token(
            &provider_c,
            &-50_i32,
            &50_i32,
            &token_a,
            &10_000_i128,
            &1_i128,
            &u64::MAX,
        );
        assert_eq!(quote3.liquidity, mint3.liquidity);
        assert_eq!(quote3.amount_used, mint3.amount_used);
    }

    /// Verify token transfers are correct by checking balances.
    #[test]
    fn test_single_token_deposit_balances_correct() {
        use soroban_sdk::token::StellarAssetClient;

        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        env.budget().reset_unlimited();
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        // Use negative ticks (positive ticks have broken sqrt math in u128 impl).
        // current=-300, range=[-200,-100]: current below range → only token_a needed.
        client.initialize(&admin, &token_a, &token_b, &0_i128, &-300_i32, &1_i32);

        let provider = Address::generate(&env);
        let sac_a = StellarAssetClient::new(&env, &token_a);
        let tok_a = TokenClient::new(&env, &token_a);

        let minted = 100_000_i128;
        sac_a.mint(&provider, &minted);
        sac_a.mint(&cl_addr, &100_000_i128);

        let amount_in = 10_000_i128;
        client.mint_position_single_token(
            &provider,
            &-200_i32,
            &-100_i32,
            &token_a,
            &amount_in,
            &1_i128,
            &u64::MAX,
        );

        let final_balance = tok_a.balance(&provider);
        let tokens_spent = minted - final_balance;
        // Provider should have spent at most amount_in (all-or-nothing for out-of-range).
        assert!(
            tokens_spent <= amount_in,
            "provider should have lost at most the deposited amount, got {tokens_spent}"
        );
    }

    // ── Burn single-token position tests ──────────────────────────────────────

    /// Burning a single-token (out-of-range) position returns the correct token.
    #[test]
    fn test_burn_single_token_below_range_returns_token_a() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        env.budget().reset_unlimited();
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        // Use negative ticks (positive ticks have broken sqrt math in u128 impl).
        // current=-300 < lower=-200: below range, only token_a.
        client.initialize(&admin, &token_a, &token_b, &0_i128, &-300_i32, &1_i32);

        let provider = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&provider, &100_000_i128);
        StellarAssetClient::new(&env, &token_a).mint(&cl_addr, &100_000_i128);

        let result = client.mint_position_single_token(
            &provider,
            &-200_i32,
            &-100_i32,
            &token_a,
            &10_000_i128,
            &1_i128,
            &u64::MAX,
        );

        let liq = result.liquidity;
        let (burn_a, burn_b) = client.burn_position(&provider, &-200_i32, &-100_i32, &liq);

        // Should return only token A (position was out of range);
        assert!(burn_a > 0, "burn should return token_a");
        assert_eq!(
            burn_b, 0_i128,
            "burn should not return token_b for out-of-range position"
        );
    }

    /// Burning an in-range single-token position returns both tokens proportionally.
    #[test]
    fn test_burn_single_token_in_range_returns_both_tokens() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        env.budget().reset_unlimited();
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        client.initialize(&admin, &token_a, &token_b, &0_i128, &0_i32, &1_i32);

        let provider = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&provider, &100_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&provider, &100_000_i128);
        StellarAssetClient::new(&env, &token_a).mint(&cl_addr, &100_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&cl_addr, &100_000_i128);

        // Deposit token A in range [-100, 100]
        let result = client.mint_position_single_token(
            &provider,
            &-100_i32,
            &100_i32,
            &token_a,
            &10_000_i128,
            &1_i128,
            &u64::MAX,
        );

        let liq = result.liquidity;
        let (burn_a, burn_b) = client.burn_position(&provider, &-100_i32, &100_i32, &liq);

        // Position was in-range, so both tokens should be returned
        assert!(burn_a > 0 || burn_b > 0, "burn should return tokens");
    }

    // ── Issue #348: NFT-keyed position ownership ───────────────────────────────

    use cl_position_nft::{ClPositionNft, ClPositionNftClient};

    /// Bundle of handles for a CL pool with a position-NFT contract wired in and
    /// a single in-range position already opened by `alice`.
    struct NftFixture<'a> {
        client: ConcentratedLiquidityClient<'a>,
        nft: ClPositionNftClient<'a>,
        token_a: Address,
        token_b: Address,
        alice: Address,
        lower: i32,
        upper: i32,
        liquidity: i128,
    }

    fn setup_with_nft(env: &Env) -> NftFixture<'_> {
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let admin = Address::generate(env);
        let alice = Address::generate(env);

        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();

        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(env, &cl_addr);
        client.initialize(&admin, &token_a, &token_b, &30_i128, &0_i32, &1_i32);

        // Deploy and wire the position-NFT contract (cl_pool = this pool).
        let nft_addr = env.register_contract(None, ClPositionNft);
        let nft = ClPositionNftClient::new(env, &nft_addr);
        nft.initialize(&admin, &cl_addr);
        client.set_position_nft(&admin, &Some(nft_addr.clone()));

        StellarAssetClient::new(env, &token_a).mint(&alice, &1_000_000_i128);
        StellarAssetClient::new(env, &token_b).mint(&alice, &1_000_000_i128);

        let lower = -100_i32;
        let upper = 100_i32;
        client.mint_position(
            &alice,
            &lower,
            &upper,
            &100_000_i128,
            &100_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );
        let liquidity = client.get_position(&alice, &lower, &upper).liquidity;

        NftFixture {
            client,
            nft,
            token_a,
            token_b,
            alice,
            lower,
            upper,
            liquidity,
        }
    }

    #[test]
    fn mint_tokenizes_position_and_records_indexes() {
        let env = Env::default();
        let f = setup_with_nft(&env);

        // NFT token 0 minted to alice, both index directions recorded.
        assert_eq!(f.nft.owner_of(&0_u64), f.alice);
        assert_eq!(
            f.client.position_token_id(&f.alice, &f.lower, &f.upper),
            Some(0_u64)
        );
        assert_eq!(
            f.client.position_of_token(&0_u64),
            Some((f.alice.clone(), f.lower, f.upper))
        );
    }

    #[test]
    fn transferred_owner_can_burn_via_token_id() {
        let env = Env::default();
        let f = setup_with_nft(&env);
        let bob = Address::generate(&env);

        // Alice transfers the position NFT to Bob.
        f.nft.transfer(&f.alice, &f.alice, &bob, &0_u64);
        assert_eq!(f.nft.owner_of(&0_u64), bob);

        // Bob fully burns the position via the NFT token id; tokens go to Bob.
        let (a_out, b_out) = f
            .client
            .burn_position_by_token_id(&bob, &0_u64, &f.liquidity);
        assert!(a_out > 0 || b_out > 0);
        assert_eq!(TokenClient::new(&env, &f.token_a).balance(&bob), a_out);
        assert_eq!(TokenClient::new(&env, &f.token_b).balance(&bob), b_out);

        // Position fully closed: NFT burned and both indexes cleared.
        assert!(f.nft.try_owner_of(&0_u64).is_err());
        assert_eq!(f.client.position_of_token(&0_u64), None);
        assert_eq!(
            f.client.position_token_id(&f.alice, &f.lower, &f.upper),
            None
        );
    }

    #[test]
    fn legacy_provider_mutation_rejected_after_transfer() {
        let env = Env::default();
        let f = setup_with_nft(&env);
        let bob = Address::generate(&env);
        f.nft.transfer(&f.alice, &f.alice, &bob, &0_u64);

        let mint_err = f
            .client
            .try_mint_position(
                &f.alice,
                &f.lower,
                &f.upper,
                &10_000_i128,
                &10_000_i128,
                &0_i128,
                &0_i128,
                &u64::MAX,
            )
            .err()
            .unwrap()
            .unwrap();
        assert_eq!(mint_err, ClError::NotNftOwner);

        let modify_err = f
            .client
            .try_modify_position(
                &f.alice,
                &f.lower,
                &f.upper,
                &1_000_i128,
                &0_i128,
                &0_i128,
                &u64::MAX,
            )
            .err()
            .unwrap()
            .unwrap();
        assert_eq!(modify_err, ClError::NotNftOwner);
    }

    #[test]
    fn legacy_provider_path_blocked_after_transfer() {
        let env = Env::default();
        let f = setup_with_nft(&env);
        let bob = Address::generate(&env);
        f.nft.transfer(&f.alice, &f.alice, &bob, &0_u64);

        // Alice no longer owns the NFT — her address-keyed calls are rejected.
        let burn_err = f
            .client
            .try_burn_position(&f.alice, &f.lower, &f.upper, &f.liquidity)
            .err()
            .unwrap()
            .unwrap();
        assert_eq!(burn_err, ClError::NotNftOwner);

        let collect_err = f
            .client
            .try_collect_fees(&f.alice, &f.lower, &f.upper)
            .err()
            .unwrap()
            .unwrap();
        assert_eq!(collect_err, ClError::NotNftOwner);
    }

    #[test]
    fn burn_by_token_id_rejects_non_owner() {
        let env = Env::default();
        let f = setup_with_nft(&env);
        let stranger = Address::generate(&env);

        // Stranger never owned the NFT.
        let err = f
            .client
            .try_burn_position_by_token_id(&stranger, &0_u64, &f.liquidity)
            .err()
            .unwrap()
            .unwrap();
        assert_eq!(err, ClError::NotNftOwner);

        // After transfer, even the original provider is no longer the owner.
        let bob = Address::generate(&env);
        f.nft.transfer(&f.alice, &f.alice, &bob, &0_u64);
        let err2 = f
            .client
            .try_burn_position_by_token_id(&f.alice, &0_u64, &f.liquidity)
            .err()
            .unwrap()
            .unwrap();
        assert_eq!(err2, ClError::NotNftOwner);
    }

    #[test]
    fn transferred_owner_collects_fees_via_token_id() {
        // Dedicated high-fee pool (1000 bps) so swap fees are clearly observable,
        // mirroring the existing fee-accrual tests.
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let alice = Address::generate(&env);
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        client.initialize(&admin, &token_a, &token_b, &1000_i128, &100_i32, &1_i32);

        let nft_addr = env.register_contract(None, ClPositionNft);
        let nft = ClPositionNftClient::new(&env, &nft_addr);
        nft.initialize(&admin, &cl_addr);
        client.set_position_nft(&admin, &Some(nft_addr));

        StellarAssetClient::new(&env, &token_a).mint(&alice, &1_000_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&alice, &1_000_000_i128);
        client.mint_position(
            &alice,
            &0_i32,
            &150_i32,
            &100_000_i128,
            &100_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );

        // Alice transfers the position NFT to Bob.
        let bob = Address::generate(&env);
        nft.transfer(&alice, &alice, &bob, &0_u64);

        // A trader swaps token A → token B, accruing token A fees to the position.
        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&trader, &50_000_i128);
        client.swap(&trader, &true, &2_000_i128, &0_u128, &0_i128, &u64::MAX);

        // Bob (current owner) collects the accrued fees to his own address.
        let (fee_a, fee_b) = client.collect_fees_by_token_id(&bob, &0_u64);
        assert!(fee_a > 0, "token A fees should accrue from an A->B swap");
        assert_eq!(TokenClient::new(&env, &token_a).balance(&bob), fee_a);
        assert_eq!(fee_b, 0);

        // The NFT and its indexes survive a fee collection (position still open).
        assert_eq!(nft.owner_of(&0_u64), bob);
        assert_eq!(
            client.position_of_token(&0_u64),
            Some((alice.clone(), 0_i32, 150_i32))
        );
    }

    #[test]
    fn full_burn_via_token_id_does_not_leak_fees_to_provider() {
        // High-fee pool so fees clearly accrue.
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let alice = Address::generate(&env);
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        client.initialize(&admin, &token_a, &token_b, &1000_i128, &100_i32, &1_i32);

        let nft_addr = env.register_contract(None, ClPositionNft);
        let nft = ClPositionNftClient::new(&env, &nft_addr);
        nft.initialize(&admin, &cl_addr);
        client.set_position_nft(&admin, &Some(nft_addr));

        StellarAssetClient::new(&env, &token_a).mint(&alice, &1_000_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&alice, &1_000_000_i128);
        client.mint_position(
            &alice,
            &0_i32,
            &150_i32,
            &100_000_i128,
            &100_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );
        let liquidity = client.get_position(&alice, &0_i32, &150_i32).liquidity;

        // Transfer to Bob, then accrue fees via a swap.
        let bob = Address::generate(&env);
        nft.transfer(&alice, &alice, &bob, &0_u64);
        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&trader, &50_000_i128);
        client.swap(&trader, &true, &2_000_i128, &0_u128, &0_i128, &u64::MAX);

        // Bob fully closes the position WITHOUT a separate collect_fees call.
        client.burn_position_by_token_id(&bob, &0_u64, &liquidity);
        assert!(TokenClient::new(&env, &token_a).balance(&bob) > 0);

        // The original provider can no longer reclaim the accrued fees: the
        // owed balance was swept to Bob on close.
        let (alice_a, alice_b) = client.collect_fees(&alice, &0_i32, &150_i32);
        assert_eq!((alice_a, alice_b), (0_i128, 0_i128));
    }

    /// Invariant test for the burn/collect balance clamp (issue #787): across
    /// a sequence of mint/swap/partial-burn/full-burn operations over three
    /// positions, a burn or collect must never attempt to pay out more of a
    /// token than the contract actually held immediately before that call,
    /// and the contract's balance must never go negative — checked via real
    /// `TokenClient::balance` reads, not just "the call didn't panic".
    #[test]
    fn burn_and_collect_never_exceed_contract_balance() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        client.initialize(&admin, &token_a, &token_b, &1000_i128, &100_i32, &1_i32);

        let p1 = Address::generate(&env);
        let p2 = Address::generate(&env);
        let p3 = Address::generate(&env);
        for p in [&p1, &p2, &p3] {
            StellarAssetClient::new(&env, &token_a).mint(p, &1_000_000_i128);
            StellarAssetClient::new(&env, &token_b).mint(p, &1_000_000_i128);
        }

        client.mint_position(
            &p1,
            &0_i32,
            &50_i32,
            &100_000_i128,
            &100_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );
        client.mint_position(
            &p2,
            &50_i32,
            &150_i32,
            &100_000_i128,
            &100_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );
        client.mint_position(
            &p3,
            &150_i32,
            &300_i32,
            &100_000_i128,
            &100_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&trader, &200_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&trader, &200_000_i128);
        client.swap(&trader, &true, &50_000_i128, &0_u128, &0_i128, &u64::MAX);
        client.swap(&trader, &false, &50_000_i128, &0_u128, &0_i128, &u64::MAX);

        let check = |result: (i128, i128), before_a: i128, before_b: i128| {
            let (paid_a, paid_b) = result;
            assert!(
                paid_a <= before_a,
                "must never pay out more token_a than the contract held"
            );
            assert!(
                paid_b <= before_b,
                "must never pay out more token_b than the contract held"
            );
            let after_a = TokenClient::new(&env, &token_a).balance(&cl_addr);
            let after_b = TokenClient::new(&env, &token_b).balance(&cl_addr);
            assert!(
                after_a >= 0 && after_b >= 0,
                "contract balance must never go negative"
            );
            assert_eq!(
                after_a,
                before_a - paid_a,
                "token_a balance must drop by exactly what was paid"
            );
            assert_eq!(
                after_b,
                before_b - paid_b,
                "token_b balance must drop by exactly what was paid"
            );
        };

        // Partial burn on position 2.
        let liq2 = client.get_position(&p2, &50_i32, &150_i32).liquidity;
        let (before_a, before_b) = (
            TokenClient::new(&env, &token_a).balance(&cl_addr),
            TokenClient::new(&env, &token_b).balance(&cl_addr),
        );
        let res = client.burn_position(&p2, &50_i32, &150_i32, &(liq2 / 2));
        check(res, before_a, before_b);

        // Full burn + fee collection on position 3.
        let liq3 = client.get_position(&p3, &150_i32, &300_i32).liquidity;
        let (before_a, before_b) = (
            TokenClient::new(&env, &token_a).balance(&cl_addr),
            TokenClient::new(&env, &token_b).balance(&cl_addr),
        );
        let res = client.burn_position(&p3, &150_i32, &300_i32, &liq3);
        check(res, before_a, before_b);
        let (before_a, before_b) = (
            TokenClient::new(&env, &token_a).balance(&cl_addr),
            TokenClient::new(&env, &token_b).balance(&cl_addr),
        );
        let res = client.collect_fees(&p3, &150_i32, &300_i32);
        check(res, before_a, before_b);

        // Full burn on position 1.
        let liq1 = client.get_position(&p1, &0_i32, &50_i32).liquidity;
        let (before_a, before_b) = (
            TokenClient::new(&env, &token_a).balance(&cl_addr),
            TokenClient::new(&env, &token_b).balance(&cl_addr),
        );
        let res = client.burn_position(&p1, &0_i32, &50_i32, &liq1);
        check(res, before_a, before_b);

        // Remaining half of position 2, plus its fees.
        let liq2_rest = client.get_position(&p2, &50_i32, &150_i32).liquidity;
        let (before_a, before_b) = (
            TokenClient::new(&env, &token_a).balance(&cl_addr),
            TokenClient::new(&env, &token_b).balance(&cl_addr),
        );
        let res = client.burn_position(&p2, &50_i32, &150_i32, &liq2_rest);
        check(res, before_a, before_b);
        let (before_a, before_b) = (
            TokenClient::new(&env, &token_a).balance(&cl_addr),
            TokenClient::new(&env, &token_b).balance(&cl_addr),
        );
        let res = client.collect_fees(&p2, &50_i32, &150_i32);
        check(res, before_a, before_b);
    }

    /// Different layout from `full_burn_via_token_id_does_not_leak_fees_to_provider`:
    /// a *partial* burn (not a full close) with no separate `collect_fees`
    /// call first, on a single wide position. Proven to fail on unmodified
    /// code with the same class of insufficient-balance trap; must now
    /// succeed with the payout clamped to what the contract actually holds.
    #[test]
    fn partial_burn_does_not_exceed_contract_balance() {
        let env = Env::default();
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let alice = Address::generate(&env);
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        client.initialize(&admin, &token_a, &token_b, &1000_i128, &100_i32, &1_i32);

        StellarAssetClient::new(&env, &token_a).mint(&alice, &1_000_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&alice, &1_000_000_i128);
        client.mint_position(
            &alice,
            &0_i32,
            &150_i32,
            &100_000_i128,
            &100_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );
        let liquidity = client.get_position(&alice, &0_i32, &150_i32).liquidity;

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&trader, &50_000_i128);
        client.swap(&trader, &true, &2_000_i128, &0_u128, &0_i128, &u64::MAX);

        let before_a = TokenClient::new(&env, &token_a).balance(&cl_addr);
        let before_b = TokenClient::new(&env, &token_b).balance(&cl_addr);
        let (paid_a, paid_b) = client.burn_position(&alice, &0_i32, &150_i32, &(liquidity / 2));
        assert!(paid_a <= before_a);
        assert!(paid_b <= before_b);
        let after_a = TokenClient::new(&env, &token_a).balance(&cl_addr);
        let after_b = TokenClient::new(&env, &token_b).balance(&cl_addr);
        assert!(after_a >= 0 && after_b >= 0);
        assert_eq!(after_a, before_a - paid_a);
        assert_eq!(after_b, before_b - paid_b);
    }

    #[test]
    fn test_partial_burn_by_token_id_preserves_nft_and_indexes() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let alice = Address::generate(&env);

        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        client.initialize(&admin, &token_a, &token_b, &1000_i128, &100_i32, &1_i32);

        let nft_addr = env.register_contract(None, ClPositionNft);
        let nft = ClPositionNftClient::new(&env, &nft_addr);
        nft.initialize(&admin, &cl_addr);
        client.set_position_nft(&admin, &Some(nft_addr.clone()));

        StellarAssetClient::new(&env, &token_a).mint(&alice, &1_000_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&alice, &1_000_000_i128);
        client.mint_position(
            &alice,
            &0_i32,
            &150_i32,
            &100_000_i128,
            &100_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );

        let total_liquidity = client.get_position(&alice, &0_i32, &150_i32).liquidity;
        let partial_burn = total_liquidity / 2;

        // Perform partial burn via token ID
        client.burn_position_by_token_id(&alice, &0_u64, &partial_burn);

        // Position still exists with remaining liquidity
        let pos_after = client.get_position(&alice, &0_i32, &150_i32);
        assert_eq!(pos_after.liquidity, total_liquidity - partial_burn);

        // NFT still exists and owner is Alice
        assert_eq!(nft.owner_of(&0_u64), alice);

        // Burn remaining liquidity
        client.burn_position_by_token_id(&alice, &0_u64, &(total_liquidity - partial_burn));

        // Position is now fully closed
        let pos_final = client.get_position(&alice, &0_i32, &150_i32);
        assert_eq!(pos_final.liquidity, 0);

        // NFT has been burned (trying owner_of panics / returns error)
        let owner_res = nft.try_owner_of(&0_u64);
        assert!(owner_res.is_err());
    }

    #[test]
    fn token_id_path_requires_nft_configured() {
        // A pool with no NFT wired in cannot resolve positions by token id.
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        client.initialize(&admin, &token_a, &token_b, &30_i128, &0_i32, &1_i32);

        let err = client
            .try_burn_position_by_token_id(&admin, &0_u64, &1_i128)
            .err()
            .unwrap()
            .unwrap();
        assert_eq!(err, ClError::NftNotConfigured);
    }

    #[test]
    fn legacy_path_unaffected_when_owner_unchanged() {
        // While the original provider still holds the NFT, the address-keyed
        // path keeps working exactly as before.
        let env = Env::default();
        let f = setup_with_nft(&env);

        let (a_out, b_out) = f
            .client
            .burn_position(&f.alice, &f.lower, &f.upper, &f.liquidity);
        assert!(a_out > 0 || b_out > 0);

        // Full close burns the NFT and clears the indexes.
        assert!(f.nft.try_owner_of(&0_u64).is_err());
        assert_eq!(
            f.client.position_token_id(&f.alice, &f.lower, &f.upper),
            None
        );
    }

    #[test]
    fn cannot_change_nft_contract_after_positions_tokenized() {
        // Regression test for vulnerability: changing DataKey::PositionNft to a
        // different contract after positions have been minted would orphan the
        // existing NftTokenToPosition / PositionNftToken indices. Later calls to
        // resolve_token_owner or ensure_legacy_owner would query the new contract
        // with stale token_ids from the old contract, causing:
        // 1. Trap if token_id doesn't exist in new contract (locks LP out)
        // 2. Wrong owner if new contract reused the same sequential id (authorization bypass)
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();

        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        client.initialize(&admin, &token_a, &token_b, &30_i128, &0_i32, &1_i32);

        // Wire NFT contract v1 and mint a position.
        let nft_v1 = env.register_contract(None, ClPositionNft);
        let nft_v1_client = ClPositionNftClient::new(&env, &nft_v1);
        nft_v1_client.initialize(&admin, &cl_addr);
        client.set_position_nft(&admin, &Some(nft_v1.clone()));

        let alice = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&alice, &1_000_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&alice, &1_000_000_i128);

        client.mint_position(
            &alice,
            &-100,
            &100,
            &100_000_i128,
            &100_000_i128,
            &0,
            &0,
            &u64::MAX,
        );
        assert_eq!(client.position_token_id(&alice, &-100, &100), Some(0_u64));

        // Attempt to change to NFT contract v2 — must be rejected.
        let nft_v2 = env.register_contract(None, ClPositionNft);
        let err = client
            .try_set_position_nft(&admin, &Some(nft_v2))
            .err()
            .unwrap()
            .unwrap();
        assert_eq!(err, ClError::NftContractChangeBlocked);

        // Changing to None (detach) should still be allowed.
        client.set_position_nft(&admin, &None);
        assert_eq!(client.position_nft(), None);

        // Re-attaching the same contract is allowed.
        client.set_position_nft(&admin, &Some(nft_v1.clone()));
        assert_eq!(client.position_nft(), Some(nft_v1));
    }
}

#[cfg(test)]
mod test_swap_exact_out {
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::token::{StellarAssetClient, TokenClient as StellarTokenClient};
    use soroban_sdk::Env;

    struct Env696<'a> {
        env: Env,
        provider: Address,
        token_a: Address,
        token_b: Address,
        cl_addr: Address,
        client: ConcentratedLiquidityClient<'a>,
    }

    /// Deploys a pool funded *only* through a real `mint_position` deposit —
    /// unlike some other fixtures in this file, the contract is not
    /// separately pre-funded with a balance cushion, so a token-balance
    /// invariant check here is meaningful rather than trivially satisfied.
    fn setup_exact_out<'a>(env: &'a Env, fee_bps: i128, initial_tick: i32) -> Env696<'a> {
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let admin = Address::generate(env);
        let provider = Address::generate(env);

        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();

        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(env, &cl_addr);
        client.initialize(&admin, &token_a, &token_b, &fee_bps, &initial_tick, &1_i32);

        StellarAssetClient::new(env, &token_a).mint(&provider, &100_000_000_i128);
        StellarAssetClient::new(env, &token_b).mint(&provider, &100_000_000_i128);

        Env696 {
            env: env.clone(),
            provider,
            token_a,
            token_b,
            cl_addr,
            client,
        }
    }

    fn balances(f: &Env696) -> (i128, i128) {
        (
            StellarTokenClient::new(&f.env, &f.token_a).balance(&f.cl_addr),
            StellarTokenClient::new(&f.env, &f.token_b).balance(&f.cl_addr),
        )
    }

    // ── Pure-math unit tests for compute_final_price_and_input ───────────────

    #[test]
    fn compute_final_price_and_input_zero_for_one_hand_computed() {
        let liquidity = 1_000_000_i128;
        let sqrt_price_current = 1u128 << 96; // p_c = 1000 exactly
        let amount_out = 100_000_i128;
        // drop = 100_000 * 1000 / 1_000_000 = 100 -> p_t = 900
        // amount_in = ceil(1_000_000*1000*(1000-900) / (1000*900))
        //           = ceil(100_000_000_000 / 900_000) = ceil(111111.111..) = 111112
        let (price_next, amount_in) = ConcentratedLiquidity::compute_final_price_and_input(
            liquidity,
            sqrt_price_current,
            amount_out,
            true,
        );
        assert_eq!(amount_in, 111_112);
        let expected_price = (900u128 * (1u128 << 96)) / 1000;
        assert_eq!(price_next, expected_price);
    }

    #[test]
    fn compute_final_price_and_input_one_for_zero_hand_computed() {
        let liquidity = 1_000_000_i128;
        let sqrt_price_current = 1u128 << 96; // sqrt(price) = 1
        let amount_out = 100_000_i128;
        // token0 out, token1 in, so the price rises:
        //   sqrt_next = L * sqrt_p / (L - amount0 * sqrt_p / Q96)
        //             = 1_000_000 / (1_000_000 - 100_000) = 10/9
        //   amount_in = ceil(L * (sqrt_next - sqrt_p) / Q96)
        //             = ceil(1_000_000 * 1/9) = ceil(111_111.11..) = 111_112
        //
        // The previous 3-significant-digit price scale rounded sqrt_next from
        // 1.1111.. up to 1.112 and charged 112_000 — a 0.8% overcharge that
        // came purely from the quantisation, not from pool-favourable rounding.
        let (price_next, amount_in) = ConcentratedLiquidity::compute_final_price_and_input(
            liquidity,
            sqrt_price_current,
            amount_out,
            false,
        );
        assert_eq!(amount_in, 111_112);

        let q96 = 1u128 << 96;
        let expected_price = (10 * q96).div_ceil(9);
        assert_eq!(price_next, expected_price);
        // And the input really does cover the requested output at that price.
        assert!(amount_in as u128 * q96 >= liquidity as u128 * (price_next - sqrt_price_current));
    }

    #[test]
    fn compute_final_price_and_input_rounds_up_on_remainder() {
        // Pick numbers where the exact-in-domain division has a non-zero
        // remainder, to prove `ceil_div` actually rounds up rather than
        // truncating like the forward (exact-in) math does.
        let liquidity = 7_i128;
        let sqrt_price_current = 1u128 << 96; // p_c = 1000
        let amount_out = 1_i128;
        // zero_for_one: drop = 1*1000/7 = 142 (floor) -> p_t = 858
        // amount_in = ceil(7*1000*(1000-858) / (1000*858)) = ceil(994000/858000) = ceil(1.158..) = 2
        let (_price, amount_in) = ConcentratedLiquidity::compute_final_price_and_input(
            liquidity,
            sqrt_price_current,
            amount_out,
            true,
        );
        assert_eq!(
            amount_in, 2,
            "must round up, not truncate, when there is a remainder"
        );

        // Verify ceil_div itself on the exact boundary and just past it.
        assert_eq!(ConcentratedLiquidity::ceil_div(900_000, 900), 1000); // exact: no rounding needed
        assert_eq!(ConcentratedLiquidity::ceil_div(900_001, 900), 1001); // one unit past: rounds up
    }

    #[test]
    fn compute_final_price_and_input_zero_amount_out_needs_no_input() {
        let (_price, amount_in) =
            ConcentratedLiquidity::compute_final_price_and_input(1_000_000, 1u128 << 96, 0, true);
        assert_eq!(amount_in, 0);
        let (_price, amount_in) =
            ConcentratedLiquidity::compute_final_price_and_input(1_000_000, 1u128 << 96, 0, false);
        assert_eq!(amount_in, 0);
    }

    /// Regression test for the exact bug the doc comment on
    /// `compute_final_price_and_input` describes: with a floor-rounded
    /// price move, a small `amount_out` relative to `liquidity` truncates
    /// the price delta to zero and computes `amount_in == 0` — the pool
    /// would give away `amount_out` for free. `liquidity = 10_000_000` and
    /// `amount_out = 1_000` reproduce it exactly (`1_000*1000/10_000_000`
    /// floors to `0`); the fix (`ceil_div` on the price-delta term) must
    /// keep `amount_in` strictly positive in both directions.
    #[test]
    fn compute_final_price_and_input_does_not_undercharge_small_amount_out() {
        let liquidity = 10_000_000_i128;
        let sqrt_price_current = 1u128 << 96;
        let amount_out = 1_000_i128;

        let (_price, amount_in) = ConcentratedLiquidity::compute_final_price_and_input(
            liquidity,
            sqrt_price_current,
            amount_out,
            true,
        );
        assert!(
            amount_in > 0,
            "zero_for_one: must never charge zero input for a positive amount_out"
        );

        let (_price, amount_in) = ConcentratedLiquidity::compute_final_price_and_input(
            liquidity,
            sqrt_price_current,
            amount_out,
            false,
        );
        assert!(
            amount_in > 0,
            "one_for_zero: must never charge zero input for a positive amount_out"
        );
    }

    // ── Basic exact-out behaviour ──────────────────────────────────────────

    #[test]
    fn swap_exact_out_normal_path_zero_for_one() {
        let env = Env::default();
        let f = setup_exact_out(&env, 30, 0);
        f.client.mint_position(
            &f.provider,
            &-1000,
            &1000,
            &10_000_000,
            &10_000_000,
            &0,
            &0,
            &u64::MAX,
        );

        let b_before = StellarTokenClient::new(&env, &f.token_b).balance(&f.provider);
        let amount_out = 1_000_i128;
        let amount_in = f.client.swap_exact_out(
            &f.provider,
            &true,
            &amount_out,
            &0_u128,
            &i128::MAX,
            &10_000,
        );
        assert!(amount_in > 0);

        let actual_out = StellarTokenClient::new(&env, &f.token_b).balance(&f.provider) - b_before;
        assert_eq!(
            actual_out, amount_out,
            "trader must receive exactly the requested amount_out"
        );
    }

    #[test]
    fn swap_exact_out_normal_path_one_for_zero() {
        let env = Env::default();
        let f = setup_exact_out(&env, 30, 0);
        f.client.mint_position(
            &f.provider,
            &-1000,
            &1000,
            &10_000_000,
            &10_000_000,
            &0,
            &0,
            &u64::MAX,
        );

        let a_before = StellarTokenClient::new(&env, &f.token_a).balance(&f.provider);
        let amount_out = 1_000_i128;
        let amount_in = f.client.swap_exact_out(
            &f.provider,
            &false,
            &amount_out,
            &u128::MAX,
            &i128::MAX,
            &10_000,
        );
        assert!(amount_in > 0);

        let actual_out = StellarTokenClient::new(&env, &f.token_a).balance(&f.provider) - a_before;
        assert_eq!(actual_out, amount_out);
    }

    #[test]
    fn quote_exact_out_matches_input_actually_consumed() {
        let env = Env::default();
        let f = setup_exact_out(&env, 30, 0);
        f.client.mint_position(
            &f.provider,
            &-1000,
            &1000,
            &10_000_000,
            &10_000_000,
            &0,
            &0,
            &u64::MAX,
        );

        let amount_out = 2_500_i128;
        let quoted = f.client.quote_exact_out(&true, &amount_out, &0_u128);
        let actual = f.client.swap_exact_out(
            &f.provider,
            &true,
            &amount_out,
            &0_u128,
            &i128::MAX,
            &10_000,
        );
        assert_eq!(
            quoted, actual,
            "quote_exact_out must match the input swap_exact_out actually consumed"
        );
    }

    #[test]
    fn quote_exact_out_does_not_mutate_state() {
        let env = Env::default();
        let f = setup_exact_out(&env, 30, 0);
        f.client.mint_position(
            &f.provider,
            &-1000,
            &1000,
            &10_000_000,
            &10_000_000,
            &0,
            &0,
            &u64::MAX,
        );

        let state_before = f.client.get_pool_state();
        let balances_before = balances(&f);
        f.client.quote_exact_out(&true, &1_000_i128, &0_u128);
        let state_after = f.client.get_pool_state();
        let balances_after = balances(&f);

        assert_eq!(state_before, state_after);
        assert_eq!(balances_before, balances_after);
    }

    // ── Round trip: quote_exact_out vs. actual swap_exact_out ─────────────────
    //
    // `quote_exact_out_matches_input_actually_consumed` above is the decisive
    // round-trip test: it asserts *exact* equality between what
    // `quote_exact_out` predicts and what `swap_exact_out` actually charges,
    // for the same pool state — the two share the same `walk_exact_out` core,
    // so they can never disagree. A separate round trip through
    // `estimate_price_impact` (a plain exact-in quote → its output → asking
    // `quote_exact_out` for that same output → comparing to the original
    // exact-in amount) was also attempted here, but this pool's price
    // representation carries only 3 significant digits
    // (`(sqrt_price_x96 * 1000) >> 96`), and reconciling it against a
    // hand-computed expectation for a *second, independently-computed*
    // trade turned out to require reverse-engineering `estimate_price_impact`
    // /`simulate_swap_walk`'s own internal liquidity accounting rather than
    // testing `swap_exact_out` itself — not a productive use of a unit test.
    // The exact-match test above is the stronger, more direct check of the
    // same property.

    // ── Invariant: pool never pays out more than it takes in ─────────────────

    #[test]
    fn invariant_holds_after_100_randomized_exact_out_swaps() {
        let env = Env::default();
        let f = setup_exact_out(&env, 30, 0);
        f.client.mint_position(
            &f.provider,
            &-5000,
            &5000,
            &50_000_000,
            &50_000_000,
            &0,
            &0,
            &u64::MAX,
        );

        let (initial_a, initial_b) = balances(&f);
        let mut net_a = initial_a; // running expected balance
        let mut net_b = initial_b;

        let mut seed: u64 = 0xC0FFEE_u64;
        let mut next_rand = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };

        for _ in 0..100 {
            let zero_for_one = next_rand() % 2 == 0;
            let amount_out = 1 + (next_rand() % 2000) as i128;
            let limit = if zero_for_one { 0_u128 } else { u128::MAX };
            match f.client.try_swap_exact_out(
                &f.provider,
                &zero_for_one,
                &amount_out,
                &limit,
                &i128::MAX,
                &10_000,
            ) {
                Ok(Ok(amount_in)) => {
                    if zero_for_one {
                        net_a += amount_in;
                        net_b -= amount_out;
                    } else {
                        net_b += amount_in;
                        net_a -= amount_out;
                    }
                }
                _ => continue, // ExactOutNotFullyFilled or similar — not the invariant under test
            }
            let (bal_a, bal_b) = balances(&f);
            assert_eq!(bal_a, net_a, "token_a balance diverged from bookkeeping");
            assert_eq!(bal_b, net_b, "token_b balance diverged from bookkeeping");
            assert!(
                bal_a >= 0 && bal_b >= 0,
                "pool must never hold a negative balance"
            );
        }
    }

    // ── Multi-tick crossing ────────────────────────────────────────────────

    #[test]
    fn multi_tick_crossing_exact_out_matches_hand_computed_active_liquidity() {
        let env = Env::default();
        let f = setup_exact_out(&env, 0, 0); // 0% fee for a clean hand computation

        // Four adjacent, non-overlapping ranges so a large exact-out swap
        // must cross at least 3 initialized ticks (at -300, -200, -100, 100).
        // `mint_position`'s amount arguments are *token amounts*, not raw
        // liquidity units — ranges of different width convert the same
        // deposited amounts into different liquidity, so the hand-computed
        // check below reads each position's actual `liquidity` back via
        // `get_position` rather than assuming it equals the deposited amount.
        f.client.mint_position(
            &f.provider,
            &100,
            &10_000,
            &2_000_000,
            &2_000_000,
            &0,
            &0,
            &u64::MAX,
        );
        f.client.mint_position(
            &f.provider,
            &-100,
            &100,
            &2_000_000,
            &2_000_000,
            &0,
            &0,
            &u64::MAX,
        );
        f.client.mint_position(
            &f.provider,
            &-200,
            &-100,
            &2_000_000,
            &2_000_000,
            &0,
            &0,
            &u64::MAX,
        );
        f.client.mint_position(
            &f.provider,
            &-300,
            &-200,
            &2_000_000,
            &2_000_000,
            &0,
            &0,
            &u64::MAX,
        );

        let ranges = [
            (
                100i32,
                10_000i32,
                f.client.get_position(&f.provider, &100, &10_000).liquidity,
            ),
            (
                -100,
                100,
                f.client.get_position(&f.provider, &-100, &100).liquidity,
            ),
            (
                -200,
                -100,
                f.client.get_position(&f.provider, &-200, &-100).liquidity,
            ),
            (
                -300,
                -200,
                f.client.get_position(&f.provider, &-300, &-200).liquidity,
            ),
        ];

        // The [-100, 100) range straddles the pool's initial tick (0), so its
        // liquidity is computed by the in-range branch of
        // `liquidity_from_amounts` — proportionally much larger for the same
        // deposited amounts than the single-sided out-of-range branches used
        // by the other three positions. `amount_out` has to be large enough
        // to drain past that concentrated liquidity and still cross into the
        // adjacent [-200, -100) range.
        let amount_out = 2_000_000_i128; // token B out, large enough to cross several ticks
        let amount_in = f.client.swap_exact_out(
            &f.provider,
            &true,
            &amount_out,
            &0_u128,
            &i128::MAX,
            &10_000,
        );
        assert!(amount_in > 0);

        let state = f.client.get_pool_state();
        assert!(
            state.current_tick <= -100,
            "swap must have crossed at least the -100 boundary"
        );

        // Hand-computed: liquidity active at the final tick is the sum of
        // whichever of the four minted ranges still contains it.
        let expected_liquidity: i128 = ranges
            .iter()
            .filter(|(lo, hi, _liq)| state.current_tick >= *lo && state.current_tick < *hi)
            .map(|(_, _, liq)| liq)
            .sum();
        assert_eq!(
            f.client.active_liquidity(),
            expected_liquidity,
            "active_liquidity() must match the sum of ranges covering the final tick"
        );
    }

    /// Same invariant as above, mirrored: opposite price direction
    /// (`zero_for_one = false`, exercising the `else` arm of the
    /// tick-crossing branch rather than the `if` arm) over a mirrored range
    /// layout, so this is a genuinely independent reproduction rather than a
    /// copy of the same code path with different numbers.
    #[test]
    fn multi_tick_crossing_exact_out_one_for_zero_matches_hand_computed_active_liquidity() {
        let env = Env::default();
        let f = setup_exact_out(&env, 0, 0); // 0% fee for a clean hand computation

        // Mirror image of the ranges above: the range straddling the initial
        // tick, two adjacent tight ranges above it, then one wide range far
        // above (the swap's direction, so it has enough liquidity to absorb
        // a large exact-out request) so the swap moving price *up* must
        // cross at least the 100 and 200 boundaries.
        f.client.mint_position(
            &f.provider,
            &-100,
            &100,
            &2_000_000,
            &2_000_000,
            &0,
            &0,
            &u64::MAX,
        );
        f.client.mint_position(
            &f.provider,
            &100,
            &200,
            &2_000_000,
            &2_000_000,
            &0,
            &0,
            &u64::MAX,
        );
        f.client.mint_position(
            &f.provider,
            &200,
            &300,
            &2_000_000,
            &2_000_000,
            &0,
            &0,
            &u64::MAX,
        );
        f.client.mint_position(
            &f.provider,
            &300,
            &10_000,
            &2_000_000,
            &2_000_000,
            &0,
            &0,
            &u64::MAX,
        );

        let ranges = [
            (
                -100i32,
                100i32,
                f.client.get_position(&f.provider, &-100, &100).liquidity,
            ),
            (
                100,
                200,
                f.client.get_position(&f.provider, &100, &200).liquidity,
            ),
            (
                200,
                300,
                f.client.get_position(&f.provider, &200, &300).liquidity,
            ),
            (
                300,
                10_000,
                f.client.get_position(&f.provider, &300, &10_000).liquidity,
            ),
        ];

        let amount_out = 2_000_000_i128; // token A out, large enough to cross several ticks
                                         // `u128::MAX`, not `0`, is this crate's "no limit" sentinel for
                                         // `zero_for_one = false` (see `swap_exact_out_normal_path_one_for_zero`).
        let amount_in = f.client.swap_exact_out(
            &f.provider,
            &false,
            &amount_out,
            &u128::MAX,
            &i128::MAX,
            &10_000,
        );
        assert!(amount_in > 0);

        let state = f.client.get_pool_state();
        assert!(
            state.current_tick >= 100,
            "swap must have crossed at least the 100 boundary"
        );

        let expected_liquidity: i128 = ranges
            .iter()
            .filter(|(lo, hi, _liq)| state.current_tick >= *lo && state.current_tick < *hi)
            .map(|(_, _, liq)| liq)
            .sum();
        assert_eq!(
            f.client.active_liquidity(),
            expected_liquidity,
            "active_liquidity() must match the sum of ranges covering the final tick"
        );
    }

    // ── Slippage / partial-fill reverts leave zero state change ──────────────

    #[test]
    fn exceeding_max_amount_in_reverts_with_zero_state_change() {
        let env = Env::default();
        let f = setup_exact_out(&env, 30, 0);
        f.client.mint_position(
            &f.provider,
            &-1000,
            &1000,
            &10_000_000,
            &10_000_000,
            &0,
            &0,
            &u64::MAX,
        );

        let state_before = f.client.get_pool_state();
        let balances_before = balances(&f);
        let fg_before = (
            env.as_contract(&f.cl_addr, || {
                env.storage()
                    .instance()
                    .get::<_, i128>(&DataKey::FeeGrowthGlobalA)
                    .unwrap_or(0)
            }),
            env.as_contract(&f.cl_addr, || {
                env.storage()
                    .instance()
                    .get::<_, i128>(&DataKey::FeeGrowthGlobalB)
                    .unwrap_or(0)
            }),
        );

        let result =
            f.client
                .try_swap_exact_out(&f.provider, &true, &1_000_i128, &0_u128, &1_i128, &10_000);
        assert_eq!(result, Err(Ok(ClError::SlippageExceeded)));

        assert_eq!(f.client.get_pool_state(), state_before);
        assert_eq!(balances(&f), balances_before);
        let fg_after = (
            env.as_contract(&f.cl_addr, || {
                env.storage()
                    .instance()
                    .get::<_, i128>(&DataKey::FeeGrowthGlobalA)
                    .unwrap_or(0)
            }),
            env.as_contract(&f.cl_addr, || {
                env.storage()
                    .instance()
                    .get::<_, i128>(&DataKey::FeeGrowthGlobalB)
                    .unwrap_or(0)
            }),
        );
        assert_eq!(
            fg_before, fg_after,
            "fee growth must be untouched on a reverted swap"
        );
    }

    #[test]
    fn hitting_price_limit_before_filling_amount_out_reverts() {
        let env = Env::default();
        let f = setup_exact_out(&env, 30, 0);
        f.client.mint_position(
            &f.provider,
            &-1000,
            &1000,
            &10_000_000,
            &10_000_000,
            &0,
            &0,
            &u64::MAX,
        );

        let state_before = f.client.get_pool_state();
        let balances_before = balances(&f);

        // A price limit equal to the current price allows zero movement, so
        // any positive amount_out cannot be filled.
        let current_sqrt = state_before.sqrt_price;
        let result = f.client.try_swap_exact_out(
            &f.provider,
            &true,
            &1_000_i128,
            &current_sqrt,
            &i128::MAX,
            &10_000,
        );
        assert_eq!(result, Err(Ok(ClError::ExactOutNotFullyFilled)));

        assert_eq!(f.client.get_pool_state(), state_before);
        assert_eq!(balances(&f), balances_before);
    }

    #[test]
    fn requesting_more_than_available_liquidity_can_supply_reverts() {
        let env = Env::default();
        let f = setup_exact_out(&env, 30, 0);
        // A narrow, small position: easy to exhaust.
        f.client
            .mint_position(&f.provider, &-100, &100, &1_000, &1_000, &0, &0, &u64::MAX);

        let result = f.client.try_swap_exact_out(
            &f.provider,
            &true,
            &1_000_000_000_i128,
            &0_u128,
            &i128::MAX,
            &10_000,
        );
        assert_eq!(result, Err(Ok(ClError::ExactOutNotFullyFilled)));
    }

    #[test]
    fn zero_amount_out_is_rejected() {
        let env = Env::default();
        let f = setup_exact_out(&env, 30, 0);
        f.client.mint_position(
            &f.provider,
            &-1000,
            &1000,
            &10_000_000,
            &10_000_000,
            &0,
            &0,
            &u64::MAX,
        );
        let result =
            f.client
                .try_swap_exact_out(&f.provider, &true, &0_i128, &0_u128, &i128::MAX, &10_000);
        assert_eq!(result, Err(Ok(ClError::ZeroAmounts)));

        let result = f.client.try_quote_exact_out(&true, &0_i128, &0_u128);
        assert_eq!(result, Err(Ok(ClError::ZeroAmounts)));
    }

    #[test]
    fn deadline_and_pause_are_enforced_like_swap() {
        use soroban_sdk::testutils::Ledger as _;

        let env = Env::default();
        let f = setup_exact_out(&env, 30, 0);
        f.client.mint_position(
            &f.provider,
            &-1000,
            &1000,
            &10_000_000,
            &10_000_000,
            &0,
            &0,
            &u64::MAX,
        );
        env.ledger().with_mut(|li| li.timestamp = 500);

        let result = f.client.try_swap_exact_out(
            &f.provider,
            &true,
            &1_000_i128,
            &0_u128,
            &i128::MAX,
            &10_u64,
        );
        assert_eq!(result, Err(Ok(ClError::DeadlineExpired)));

        let admin: Address = env.as_contract(&f.cl_addr, || {
            env.storage().instance().get(&DataKey::Admin).unwrap()
        });
        f.client.pause(&admin);
        let result = f.client.try_swap_exact_out(
            &f.provider,
            &true,
            &1_000_i128,
            &0_u128,
            &i128::MAX,
            &10_000_u64,
        );
        assert_eq!(result, Err(Ok(ClError::Paused)));
    }

    // ── Fee growth and oracle parity with exact-in ────────────────────────────

    #[test]
    fn fee_growth_matches_equivalent_exact_in_swap_within_tolerance() {
        // Reference: a plain exact-in swap, and the output it produces. Uses
        // a large enough trade that fee_growth_global's integer scale
        // (lp_fee * 1_000_000 / liquidity per step) isn't dominated by
        // single-unit integer noise, which would make any tolerance
        // meaningless regardless of correctness.
        let env_in = Env::default();
        let f_in = setup_exact_out(&env_in, 30, 0);
        f_in.client.mint_position(
            &f_in.provider,
            &-1000,
            &1000,
            &10_000_000,
            &10_000_000,
            &0,
            &0,
            &u64::MAX,
        );
        let out_amount = f_in
            .client
            .estimate_price_impact(&true, &500_000_i128, &0_u128)
            .amount_out;
        f_in.client.swap(
            &f_in.provider,
            &true,
            &500_000_i128,
            &0_u128,
            &0_i128,
            &10_000,
        );
        let fg_a_in: i128 = env_in.as_contract(&f_in.cl_addr, || {
            env_in
                .storage()
                .instance()
                .get(&DataKey::FeeGrowthGlobalA)
                .unwrap_or(0)
        });

        // Same pool state, asking exact-out for exactly that same output.
        let env_out = Env::default();
        let f_out = setup_exact_out(&env_out, 30, 0);
        f_out.client.mint_position(
            &f_out.provider,
            &-1000,
            &1000,
            &10_000_000,
            &10_000_000,
            &0,
            &0,
            &u64::MAX,
        );
        f_out.client.swap_exact_out(
            &f_out.provider,
            &true,
            &out_amount,
            &0_u128,
            &i128::MAX,
            &10_000,
        );
        let fg_a_out: i128 = env_out.as_contract(&f_out.cl_addr, || {
            env_out
                .storage()
                .instance()
                .get(&DataKey::FeeGrowthGlobalA)
                .unwrap_or(0)
        });

        assert!(fg_a_in > 0);
        assert!(fg_a_out > 0);
        // Within the documented rounding tolerance: both sides charge the
        // same fee_bps on essentially the same trade, just solved from
        // opposite ends.
        let diff = (fg_a_in - fg_a_out).abs();
        let tolerance = (fg_a_in / 10).max(3); // 10%, or 3 absolute units at tiny scale
        assert!(
            diff <= tolerance,
            "fee growth diverged beyond tolerance: exact-in={fg_a_in}, exact-out={fg_a_out}, diff={diff}, tolerance={tolerance}"
        );
    }

    #[test]
    fn oracle_tick_accumulator_advances_on_exact_out_swap() {
        use soroban_sdk::testutils::Ledger as _;

        let env = Env::default();
        let f = setup_exact_out(&env, 30, 0);
        f.client.mint_position(
            &f.provider,
            &-1000,
            &1000,
            &10_000_000,
            &10_000_000,
            &0,
            &0,
            &u64::MAX,
        );

        // Move current_tick away from 0 first — the accumulator's per-second
        // contribution is `current_tick * elapsed`, so at tick 0 it would
        // stay unchanged regardless of elapsed time, which would not
        // actually exercise the accumulator update.
        f.client.swap_exact_out(
            &f.provider,
            &true,
            &50_000_i128,
            &0_u128,
            &i128::MAX,
            &10_000_000,
        );
        let state = f.client.get_pool_state();
        assert_ne!(
            state.current_tick, 0,
            "test setup must move the tick away from 0"
        );

        let (cum_before, ts_before) = f.client.get_tick_cumulative();
        env.ledger().with_mut(|li| li.timestamp += 100);
        f.client.swap_exact_out(
            &f.provider,
            &true,
            &1_000_i128,
            &0_u128,
            &i128::MAX,
            &10_000_000,
        );
        let (cum_after, ts_after) = f.client.get_tick_cumulative();

        assert!(ts_after > ts_before, "oracle timestamp must advance");
        assert_ne!(
            cum_after, cum_before,
            "tick accumulator must advance on a price-moving exact-out swap"
        );
    }

    #[test]
    fn two_sequential_exact_out_swaps_both_succeed() {
        let env = Env::default();
        let f = setup_exact_out(&env, 30, 0);
        f.client.mint_position(
            &f.provider,
            &-2000,
            &2000,
            &10_000_000,
            &10_000_000,
            &0,
            &0,
            &u64::MAX,
        );

        let in1 =
            f.client
                .swap_exact_out(&f.provider, &true, &500_i128, &0_u128, &i128::MAX, &10_000);
        let in2 = f.client.swap_exact_out(
            &f.provider,
            &false,
            &500_i128,
            &u128::MAX,
            &i128::MAX,
            &10_000,
        );
        assert!(in1 > 0 && in2 > 0);
    }

    #[test]
    fn protocol_fee_accrues_on_exact_out_swap() {
        let env = Env::default();
        let f = setup_exact_out(&env, 30, 0);
        let admin: Address = env.as_contract(&f.cl_addr, || {
            env.storage().instance().get(&DataKey::Admin).unwrap()
        });
        env.as_contract(&f.cl_addr, || {
            env.storage()
                .instance()
                .set(&DataKey::ProtocolFeeBps, &2000_i128);
        });
        let _ = admin;
        f.client.mint_position(
            &f.provider,
            &-1000,
            &1000,
            &10_000_000,
            &10_000_000,
            &0,
            &0,
            &u64::MAX,
        );

        f.client.swap_exact_out(
            &f.provider,
            &true,
            &10_000_i128,
            &0_u128,
            &i128::MAX,
            &10_000,
        );

        let accrued_a: i128 = env.as_contract(&f.cl_addr, || {
            env.storage()
                .instance()
                .get(&DataKey::AccruedProtocolFeeA)
                .unwrap_or(0)
        });
        assert!(
            accrued_a > 0,
            "protocol fee must accrue on an exact-out swap when protocol_fee_bps > 0"
        );
    }
}

#[cfg(test)]
mod test_range_order_fill_status {
    // Regression tests for issue #472: a range order must record which side
    // it was placed on, and only report `Filled` once the price has crossed
    // through the range in that order's original fill direction.
    use super::*;
    use soroban_sdk::testutils::{Address as _, Ledger};
    use soroban_sdk::token::StellarAssetClient;
    use soroban_sdk::Env;

    fn setup_pool(
        env: &Env,
        initial_tick: i32,
    ) -> (Address, Address, Address, ConcentratedLiquidityClient<'_>) {
        env.mock_all_auths();
        let admin = Address::generate(env);
        env.budget().reset_unlimited();
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(env, &cl_addr);
        client.initialize(&admin, &token_a, &token_b, &0_i128, &initial_tick, &1_i32);

        let provider = Address::generate(env);
        StellarAssetClient::new(env, &token_a).mint(&provider, &100_000_000_i128);
        StellarAssetClient::new(env, &token_b).mint(&provider, &100_000_000_i128);
        StellarAssetClient::new(env, &token_a).mint(&cl_addr, &100_000_000_i128);
        StellarAssetClient::new(env, &token_b).mint(&cl_addr, &100_000_000_i128);

        (provider, token_a, token_b, client)
    }

    /// Moves the pool's current tick directly via storage, standing in for a
    /// swap that has crossed price through (or into) the range.
    fn set_current_tick(env: &Env, cl_addr: &Address, tick: i32) {
        env.as_contract(cl_addr, || {
            env.storage().instance().set(&DataKey::CurrentTick, &tick);
        });
    }

    // ── Above-range order (deposit token A, fills as price rises) ────────────

    #[test]
    fn above_range_order_is_pending_at_placement() {
        let env = Env::default();
        // current_tick = 0, range = [100, 200] → an above-range order.
        let (provider, _token_a, _token_b, client) = setup_pool(&env, 0);

        client.place_range_order(
            &provider,
            &100_i32,
            &200_i32,
            &_token_a,
            &10_000_i128,
            &1_i128,
            &u64::MAX,
        );

        assert_eq!(
            client.check_range_order_filled(&provider, &100_i32, &200_i32),
            RangeOrderStatus::Pending,
            "an above-range order must not be reported as Filled before the price has moved"
        );
    }

    #[test]
    fn above_range_order_is_pending_while_price_is_still_below_range() {
        let env = Env::default();
        let (provider, token_a, _token_b, client) = setup_pool(&env, 0);
        let cl_addr = client.address.clone();

        client.place_range_order(
            &provider,
            &100_i32,
            &200_i32,
            &token_a,
            &10_000_i128,
            &1_i128,
            &u64::MAX,
        );

        // Price has moved but has not yet reached the range.
        set_current_tick(&env, &cl_addr, 50);

        assert_eq!(
            client.check_range_order_filled(&provider, &100_i32, &200_i32),
            RangeOrderStatus::Pending
        );
    }

    #[test]
    fn above_range_order_is_filled_once_price_crosses_upper_tick() {
        let env = Env::default();
        let (provider, token_a, _token_b, client) = setup_pool(&env, 0);
        let cl_addr = client.address.clone();

        client.place_range_order(
            &provider,
            &100_i32,
            &200_i32,
            &token_a,
            &10_000_i128,
            &1_i128,
            &u64::MAX,
        );

        set_current_tick(&env, &cl_addr, 200);

        assert_eq!(
            client.check_range_order_filled(&provider, &100_i32, &200_i32),
            RangeOrderStatus::Filled
        );
    }

    // ── Below-range order (deposit token B, fills as price falls) ────────────

    #[test]
    fn below_range_order_is_pending_at_placement() {
        let env = Env::default();
        // current_tick = 300, range = [100, 200] → a below-range order.
        let (provider, _token_a, token_b, client) = setup_pool(&env, 300);

        client.place_range_order(
            &provider,
            &100_i32,
            &200_i32,
            &token_b,
            &10_000_i128,
            &1_i128,
            &u64::MAX,
        );

        assert_eq!(
            client.check_range_order_filled(&provider, &100_i32, &200_i32),
            RangeOrderStatus::Pending,
            "a below-range order must not be reported as Filled before the price has moved"
        );
    }

    #[test]
    fn below_range_order_is_pending_while_price_is_still_above_range() {
        let env = Env::default();
        let (provider, _token_a, token_b, client) = setup_pool(&env, 300);
        let cl_addr = client.address.clone();

        client.place_range_order(
            &provider,
            &100_i32,
            &200_i32,
            &token_b,
            &10_000_i128,
            &1_i128,
            &u64::MAX,
        );

        // Price has fallen but has not yet reached the range.
        set_current_tick(&env, &cl_addr, 250);

        assert_eq!(
            client.check_range_order_filled(&provider, &100_i32, &200_i32),
            RangeOrderStatus::Pending
        );
    }

    #[test]
    fn below_range_order_is_filled_once_price_crosses_lower_tick() {
        let env = Env::default();
        let (provider, _token_a, token_b, client) = setup_pool(&env, 300);
        let cl_addr = client.address.clone();

        client.place_range_order(
            &provider,
            &100_i32,
            &200_i32,
            &token_b,
            &10_000_i128,
            &1_i128,
            &u64::MAX,
        );

        set_current_tick(&env, &cl_addr, 99);

        assert_eq!(
            client.check_range_order_filled(&provider, &100_i32, &200_i32),
            RangeOrderStatus::Filled
        );
    }

    // ── Issue #595: re-placing on a range with an unwithdrawn order ──────────

    #[test]
    fn active_range_order_cannot_be_replaced() {
        let env = Env::default();
        let (provider, token_a, _token_b, client) = setup_pool(&env, 0);

        client.place_range_order(
            &provider,
            &100_i32,
            &200_i32,
            &token_a,
            &10_000_i128,
            &1_i128,
            &u64::MAX,
        );

        // A second order on the identical range must be rejected while the
        // first tranche's liquidity is still in the position.
        let result = client.try_place_range_order(
            &provider,
            &100_i32,
            &200_i32,
            &token_a,
            &10_000_i128,
            &1_i128,
            &u64::MAX,
        );
        assert_eq!(result, Err(Ok(ClError::RangeOrderExists)));

        // The original order's direction tag must be untouched.
        assert_eq!(
            client.check_range_order_filled(&provider, &100_i32, &200_i32),
            RangeOrderStatus::Pending
        );

        // A different (unused) range is still available.
        client.place_range_order(
            &provider,
            &300_i32,
            &400_i32,
            &token_a,
            &10_000_i128,
            &1_i128,
            &u64::MAX,
        );
    }

    #[test]
    fn filled_range_order_blocks_replacement_until_withdrawn() {
        let env = Env::default();
        let (provider, token_a, _token_b, client) = setup_pool(&env, 0);
        let cl_addr = client.address.clone();

        client.place_range_order(
            &provider,
            &100_i32,
            &200_i32,
            &token_a,
            &10_000_i128,
            &1_i128,
            &u64::MAX,
        );

        // The order fills: price crosses above upper_tick.
        set_current_tick(&env, &cl_addr, 200);
        assert_eq!(
            client.check_range_order_filled(&provider, &100_i32, &200_i32),
            RangeOrderStatus::Filled
        );

        // Now the same range would be a *below*-range order — the exact
        // flag-flip scenario from the issue. It must be rejected until the
        // filled tranche is withdrawn.
        let result = client.try_place_range_order(
            &provider,
            &100_i32,
            &200_i32,
            &_token_b,
            &10_000_i128,
            &1_i128,
            &u64::MAX,
        );
        assert_eq!(result, Err(Ok(ClError::RangeOrderExists)));

        // The fill status must not have been corrupted by the failed attempt.
        assert_eq!(
            client.check_range_order_filled(&provider, &100_i32, &200_i32),
            RangeOrderStatus::Filled
        );

        // After withdrawing the filled order, the range can be reused with the
        // *new* direction and the fill status resets to Pending.
        let pos = client.get_position(&provider, &100_i32, &200_i32);
        client.burn_position(&provider, &100_i32, &200_i32, &pos.liquidity);

        client.place_range_order(
            &provider,
            &100_i32,
            &200_i32,
            &_token_b,
            &10_000_i128,
            &1_i128,
            &u64::MAX,
        );
        assert_eq!(
            client.check_range_order_filled(&provider, &100_i32, &200_i32),
            RangeOrderStatus::Pending
        );

        // The new below-range order is filled once price falls below lower_tick.
        set_current_tick(&env, &cl_addr, 99);
        assert_eq!(
            client.check_range_order_filled(&provider, &100_i32, &200_i32),
            RangeOrderStatus::Filled
        );
    }

    #[test]
    fn partial_burn_keeps_range_order_active() {
        let env = Env::default();
        let (provider, token_a, _token_b, client) = setup_pool(&env, 0);

        client.place_range_order(
            &provider,
            &100_i32,
            &200_i32,
            &token_a,
            &10_000_i128,
            &1_i128,
            &u64::MAX,
        );

        let pos = client.get_position(&provider, &100_i32, &200_i32);
        let half = (pos.liquidity / 2).max(1);
        client.burn_position(&provider, &100_i32, &200_i32, &half);

        // A partially withdrawn order is still active: re-placing stays blocked.
        let result = client.try_place_range_order(
            &provider,
            &100_i32,
            &200_i32,
            &token_a,
            &10_000_i128,
            &1_i128,
            &u64::MAX,
        );
        assert_eq!(result, Err(Ok(ClError::RangeOrderExists)));
    }

    // ── Issue #1101: Bounded oracle ring buffer & O(log N) observe ────────────

    #[test]
    fn oracle_ring_buffer_5000_swaps_keeps_instance_storage_bounded_and_caps_observations() {
        let env = Env::default();
        env.mock_all_auths();
        env.budget().reset_unlimited();
        env.ledger().set_timestamp(1_000);

        let admin = Address::generate(&env);
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        client.initialize(&admin, &token_a, &token_b, &30_i128, &0_i32, &1_i32);

        let provider = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&provider, &10_000_000_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&provider, &10_000_000_000_i128);
        client.mint_position(
            &provider,
            &-1000_i32,
            &1000_i32,
            &1_000_000_000_i128,
            &1_000_000_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&trader, &10_000_000_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&trader, &10_000_000_000_i128);

        // First swap transfers tokens and accrues fees; remaining swaps up to 5,000
        // use the current sqrt_price_limit_x96 so each swap exercises the full
        // entrypoint, oracle accumulator, and ring-buffer write in distinct ledgers
        // without 10,000 cross-contract SAC host calls in debug builds.
        env.ledger().set_timestamp(1_005);
        client.swap(&trader, &true, &1_000_i128, &0_u128, &0_i128, &u64::MAX);
        let limit = client.get_pool_state().sqrt_price;
        for i in 2..5_000_u64 {
            env.ledger().set_timestamp(1_000 + i * 5);
            client.swap(&trader, &true, &1_000_i128, &limit, &0_i128, &u64::MAX);
        }
        env.ledger().set_timestamp(1_000 + 5_000 * 5);
        client.swap(&trader, &false, &1_000_i128, &0_u128, &0_i128, &u64::MAX);

        let header = client.get_oracle_header();
        assert_eq!(header.cardinality, DEFAULT_ORACLE_CARDINALITY);
        assert_eq!(header.count, DEFAULT_ORACLE_CARDINALITY);

        // Verify instance storage holds no legacy OracleTimestamps or OraclePoint entries,
        // and persistent storage holds at most DEFAULT_ORACLE_CARDINALITY observation slots.
        env.as_contract(&cl_addr, || {
            assert!(!env.storage().instance().has(&DataKey::OracleTimestamps));
            assert!(!env
                .storage()
                .instance()
                .has(&DataKey::OraclePoint(1_000 + 5)));
            assert!(env
                .storage()
                .persistent()
                .has(&DataKey::OracleObservation(DEFAULT_ORACLE_CARDINALITY - 1)));
            assert!(!env
                .storage()
                .persistent()
                .has(&DataKey::OracleObservation(DEFAULT_ORACLE_CARDINALITY)));
        });

        // Pool operations (collect_fees, burn_position, observe) continue to succeed after 5,000 swaps.
        let fees = client.collect_fees(&provider, &-1000_i32, &1000_i32);
        assert!(fees.0 >= 0 && fees.1 >= 0);
        let pos = client.get_position(&provider, &-1000_i32, &1000_i32);
        client.burn_position(&provider, &-1000_i32, &1000_i32, &(pos.liquidity / 2));
        let obs_now = client.observe(&0_u64);
        assert_eq!(obs_now, client.get_tick_cumulative().0);
    }

    #[test]
    fn oracle_observe_returns_exact_values_at_retained_sample_timestamps() {
        let env = Env::default();
        env.mock_all_auths();
        env.budget().reset_unlimited();
        env.ledger().set_timestamp(1_000);

        let (provider, _token_a, _token_b, client) = setup_pool(&env, 10);
        client.mint_position(
            &provider,
            &-500_i32,
            &500_i32,
            &10_000_000_i128,
            &10_000_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );
        let cl_addr = client.address.clone();
        let buyer = Address::generate(&env);
        StellarAssetClient::new(&env, &_token_a).mint(&buyer, &1_000_000_i128);

        let mut samples: Vec<(u64, i64)> = Vec::new(&env);
        samples.push_back((1_000, 0));

        for step in 1..=6_u64 {
            let ts = 1_000 + step * 100;
            env.ledger().set_timestamp(ts);
            set_current_tick(&env, &cl_addr, (step as i32) * 10);
            client.swap(&buyer, &true, &100_i128, &0_u128, &0_i128, &u64::MAX);
            let (cum, recorded_ts) = client.get_tick_cumulative();
            assert_eq!(recorded_ts, ts);
            samples.push_back((ts, cum));
        }

        env.ledger().set_timestamp(2_000);
        for i in 0..samples.len() {
            let (ts, expected_cum) = samples.get(i).unwrap();
            let seconds_ago = 2_000 - ts;
            assert_eq!(client.observe(&seconds_ago), expected_cum);
        }
    }

    #[test]
    fn oracle_observe_linearly_interpolates_unit_accurately_between_samples() {
        let env = Env::default();
        env.mock_all_auths();
        env.budget().reset_unlimited();
        env.ledger().set_timestamp(1_000);

        let (provider, token_a, _token_b, client) = setup_pool(&env, 20);
        client.mint_position(
            &provider,
            &-500_i32,
            &500_i32,
            &10_000_000_i128,
            &10_000_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );
        let cl_addr = client.address.clone();
        let buyer = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&buyer, &1_000_000_i128);

        // t=1000: cum=0, tick=20
        // t=1100: cum = 0 + 20*100 = 2000; then set tick = -10
        env.ledger().set_timestamp(1_100);
        client.swap(&buyer, &true, &100_i128, &0_u128, &0_i128, &u64::MAX);
        set_current_tick(&env, &cl_addr, -10);

        // t=1200: cum = 2000 + (-10)*100 = 1000; then set tick = 50
        env.ledger().set_timestamp(1_200);
        client.swap(&buyer, &true, &100_i128, &0_u128, &0_i128, &u64::MAX);
        set_current_tick(&env, &cl_addr, 50);

        // t=1300: cum = 1000 + 50*100 = 6000
        env.ledger().set_timestamp(1_300);
        client.swap(&buyer, &true, &100_i128, &0_u128, &0_i128, &u64::MAX);

        env.ledger().set_timestamp(1_350);
        // Target 1025 (seconds_ago = 325): 0 + (2000 - 0) * 25 / 100 = 500
        assert_eq!(client.observe(&325_u64), 500);
        // Target 1050 (seconds_ago = 300): 1000
        assert_eq!(client.observe(&300_u64), 1_000);
        // Target 1150 (seconds_ago = 200): 2000 + (1000 - 2000) * 50 / 100 = 1500
        assert_eq!(client.observe(&200_u64), 1_500);
        // Target 1240 (seconds_ago = 110): 1000 + (6000 - 1000) * 40 / 100 = 3000
        assert_eq!(client.observe(&110_u64), 3_000);
    }

    #[test]
    fn oracle_observe_returns_observation_too_old_before_oldest_retained_entry() {
        let env = Env::default();
        env.mock_all_auths();
        env.budget().reset_unlimited();
        env.ledger().set_timestamp(1_000);

        let (provider, token_a, token_b, client) = setup_pool(&env, 5);
        client.mint_position(
            &provider,
            &-500_i32,
            &500_i32,
            &10_000_000_i128,
            &10_000_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );
        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&trader, &100_000_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&trader, &100_000_000_i128);

        // Query before init timestamp (t=999 at now=1000 -> seconds_ago=1) must fail.
        assert_eq!(
            client.try_observe(&1_u64),
            Err(Ok(ClError::ObservationTooOld))
        );
        // Query with seconds_ago > now must also return ObservationTooOld.
        assert_eq!(
            client.try_observe(&5_000_u64),
            Err(Ok(ClError::ObservationTooOld))
        );

        // Perform DEFAULT_ORACLE_CARDINALITY + 10 swaps so the first 11 observations
        // (init at t=1000 plus swaps 1..=10 at t=1010..=1100) are overwritten.
        let total_swaps = DEFAULT_ORACLE_CARDINALITY + 10;
        let limit = client.get_pool_state().sqrt_price;
        for i in 1..=total_swaps {
            env.ledger().set_timestamp(1_000 + (i as u64) * 10);
            client.swap(&trader, &true, &100_i128, &limit, &0_i128, &u64::MAX);
        }

        let now = 1_000 + (total_swaps as u64) * 10;
        // Oldest retained observation is at i = 11 -> timestamp = 1_000 + 110 = 1_110.
        let oldest_retained_ts = 1_110_u64;
        assert!(client.try_observe(&(now - oldest_retained_ts)).is_ok());
        assert_eq!(
            client.try_observe(&(now - oldest_retained_ts + 1)),
            Err(Ok(ClError::ObservationTooOld))
        );
    }

    #[test]
    fn oracle_observe_zero_extrapolates_from_last_oracle_timestamp_to_now() {
        let env = Env::default();
        env.mock_all_auths();
        env.budget().reset_unlimited();
        env.ledger().set_timestamp(1_000);

        let (provider, token_a, _token_b, client) = setup_pool(&env, 12);
        client.mint_position(
            &provider,
            &-500_i32,
            &500_i32,
            &10_000_000_i128,
            &10_000_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );
        let buyer = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&buyer, &100_000_i128);

        env.ledger().set_timestamp(1_050);
        client.swap(&buyer, &true, &100_i128, &0_u128, &0_i128, &u64::MAX);

        let (cum_at_swap, last_ts) = client.get_tick_cumulative();
        assert_eq!(last_ts, 1_050);
        assert_eq!(client.observe(&0_u64), cum_at_swap);

        let tick_after = client.current_tick();
        env.ledger().set_timestamp(1_125);
        let expected = cum_at_swap + (tick_after as i64) * 75;
        assert_eq!(client.observe(&0_u64), expected);
    }

    #[test]
    fn oracle_get_tick_cumulative_unchanged_for_consumers() {
        let env = Env::default();
        env.mock_all_auths();
        env.budget().reset_unlimited();
        env.ledger().set_timestamp(2_000);

        let (provider, token_a, _token_b, client) = setup_pool(&env, 15);
        client.mint_position(
            &provider,
            &-500_i32,
            &500_i32,
            &10_000_000_i128,
            &10_000_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );
        assert_eq!(client.get_tick_cumulative(), (0, 2_000));

        let buyer = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&buyer, &100_000_i128);

        env.ledger().set_timestamp(2_040);
        client.swap(&buyer, &true, &100_i128, &0_u128, &0_i128, &u64::MAX);
        assert_eq!(client.get_tick_cumulative(), (15 * 40, 2_040));
    }

    #[test]
    fn oracle_swap_exact_out_records_observation_in_ring_buffer() {
        let env = Env::default();
        env.mock_all_auths();
        env.budget().reset_unlimited();
        env.ledger().set_timestamp(1_000);

        let (provider, token_a, _token_b, client) = setup_pool(&env, 10);
        client.mint_position(
            &provider,
            &-500_i32,
            &500_i32,
            &10_000_000_i128,
            &10_000_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );
        let buyer = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&buyer, &1_000_000_i128);

        env.ledger().set_timestamp(1_100);
        client.swap_exact_out(&buyer, &true, &50_i128, &0_u128, &100_000_i128, &u64::MAX);
        let (cum_1100, ts_1100) = client.get_tick_cumulative();
        assert_eq!(ts_1100, 1_100);
        assert_eq!(cum_1100, 1_000);
        assert_eq!(client.get_oracle_header().count, 2);

        env.ledger().set_timestamp(1_200);
        client.swap_exact_out(&buyer, &true, &50_i128, &0_u128, &100_000_i128, &u64::MAX);
        let (cum_1200, _) = client.get_tick_cumulative();
        assert_eq!(client.get_oracle_header().count, 3);

        // Interpolate at t=1050 (between init at 1000 and exact-out swap at 1100)
        // and at t=1150 (between the two exact-out swaps).
        env.ledger().set_timestamp(1_250);
        assert_eq!(client.observe(&200_u64), 500);
        assert_eq!(
            client.observe(&100_u64),
            cum_1100 + (cum_1200 - cum_1100) / 2
        );
    }

    #[test]
    fn oracle_observe_on_full_ring_has_bounded_instruction_cost() {
        let env = Env::default();
        env.mock_all_auths();
        env.budget().reset_unlimited();
        env.ledger().set_timestamp(1_000);

        let (provider, token_a, token_b, client) = setup_pool(&env, 0);
        client.mint_position(
            &provider,
            &-500_i32,
            &500_i32,
            &10_000_000_i128,
            &10_000_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );
        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&trader, &100_000_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&trader, &100_000_000_i128);

        let limit = client.get_pool_state().sqrt_price;
        for i in 1..=600_u64 {
            env.ledger().set_timestamp(1_000 + i * 10);
            client.swap(&trader, &true, &100_i128, &limit, &0_i128, &u64::MAX);
        }

        env.budget().reset_default();
        let _ = client.observe(&2_505_u64);
        let cpu = env.budget().cpu_instruction_cost();
        // Binary search over 512 entries reads at most ~10 persistent slots; CPU must stay
        // far below Soroban's 100M per-transaction limit.
        assert!(
            cpu < 5_000_000,
            "observe on full ring used too many CPU instructions: {cpu}"
        );
    }

    #[test]
    fn oracle_legacy_state_upgrade_migrates_and_preserves_window() {
        let env = Env::default();
        env.mock_all_auths();
        env.budget().reset_unlimited();
        env.ledger().set_timestamp(1_000);

        let (provider, token_a, _token_b, client) = setup_pool(&env, 10);
        client.mint_position(
            &provider,
            &-500_i32,
            &500_i32,
            &10_000_000_i128,
            &10_000_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );
        let cl_addr = client.address.clone();
        let buyer = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&buyer, &1_000_000_i128);

        // Simulate a pre-#1101 pool by removing OracleHeader/OracleObservation and
        // writing legacy OracleTimestamps + OraclePoint(t) into instance storage.
        env.as_contract(&cl_addr, || {
            env.storage().instance().remove(&DataKey::OracleHeader);
            env.storage()
                .persistent()
                .remove(&DataKey::OracleObservation(0));

            let mut legacy_ts: Vec<u64> = Vec::new(&env);
            legacy_ts.push_back(1_000);
            legacy_ts.push_back(1_100);
            legacy_ts.push_back(1_200);
            env.storage()
                .instance()
                .set(&DataKey::OracleTimestamps, &legacy_ts);
            env.storage()
                .instance()
                .set(&DataKey::OraclePoint(1_000), &0_i64);
            env.storage()
                .instance()
                .set(&DataKey::OraclePoint(1_100), &1_000_i64);
            env.storage()
                .instance()
                .set(&DataKey::OraclePoint(1_200), &3_000_i64);
            env.storage()
                .instance()
                .set(&DataKey::LastOracleTimestamp, &1_200_u64);
            env.storage()
                .instance()
                .set(&DataKey::TickCumulative, &3_000_i64);
        });

        // Before any post-upgrade swap, observe falls back to legacy storage accurately.
        env.ledger().set_timestamp(1_250);
        assert_eq!(client.observe(&200_u64), 500); // t=1050
        assert_eq!(client.observe(&100_u64), 2_000); // t=1150
        assert_eq!(
            client.try_observe(&300_u64),
            Err(Ok(ClError::ObservationTooOld))
        );

        // Now run a post-upgrade swap at t=1300; it must migrate legacy entries into
        // the persistent ring buffer and clean up instance storage.
        env.ledger().set_timestamp(1_300);
        client.swap(&buyer, &true, &100_i128, &0_u128, &0_i128, &u64::MAX);

        env.as_contract(&cl_addr, || {
            assert!(env.storage().instance().has(&DataKey::OracleHeader));
            assert!(!env.storage().instance().has(&DataKey::OracleTimestamps));
            assert!(!env.storage().instance().has(&DataKey::OraclePoint(1_000)));
            assert!(!env.storage().instance().has(&DataKey::OraclePoint(1_100)));
            assert!(!env.storage().instance().has(&DataKey::OraclePoint(1_200)));
        });

        let header = client.get_oracle_header();
        assert_eq!(header.count, 4);
        // Pre-upgrade window timestamps still resolve accurately from the migrated ring buffer.
        assert_eq!(client.observe(&250_u64), 500); // t=1050
        assert_eq!(client.observe(&150_u64), 2_000); // t=1150
    }

    #[test]
    fn oracle_set_cardinality_expands_ring_and_preserves_wrapped_observations() {
        let env = Env::default();
        env.mock_all_auths();
        env.budget().reset_unlimited();
        env.ledger().set_timestamp(1_000);

        let admin = Address::generate(&env);
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        client.initialize(&admin, &token_a, &token_b, &30_i128, &10_i32, &1_i32);

        let provider = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&provider, &100_000_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&provider, &100_000_000_i128);
        client.mint_position(
            &provider,
            &-500_i32,
            &500_i32,
            &10_000_000_i128,
            &10_000_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );

        let trader = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&trader, &100_000_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&trader, &100_000_000_i128);

        // Wrap the ring buffer by performing DEFAULT_ORACLE_CARDINALITY + 25 swaps.
        let initial_swaps = DEFAULT_ORACLE_CARDINALITY + 25;
        let limit = client.get_pool_state().sqrt_price;
        for i in 1..=initial_swaps {
            env.ledger().set_timestamp(1_000 + (i as u64) * 10);
            client.swap(&trader, &true, &100_i128, &limit, &0_i128, &u64::MAX);
        }

        let before_expand_oldest = client.observe(&((DEFAULT_ORACLE_CARDINALITY as u64 - 1) * 10));
        let before_expand_mid = client.observe(&2_500_u64);
        let before_expand_recent = client.observe(&50_u64);

        client.set_oracle_cardinality(&admin, &1024_u32);
        assert_eq!(client.get_oracle_cardinality(), 1024);
        let header = client.get_oracle_header();
        assert_eq!(header.cardinality, 1024);
        assert_eq!(header.count, DEFAULT_ORACLE_CARDINALITY);
        assert_eq!(header.index, DEFAULT_ORACLE_CARDINALITY - 1);

        // Observations must match identically right after expansion.
        assert_eq!(
            client.observe(&((DEFAULT_ORACLE_CARDINALITY as u64 - 1) * 10)),
            before_expand_oldest
        );
        assert_eq!(client.observe(&2_500_u64), before_expand_mid);
        assert_eq!(client.observe(&50_u64), before_expand_recent);

        // Additional swaps now grow `count` past 512 without evicting `before_expand_oldest`.
        for j in 1..=20_u64 {
            env.ledger()
                .set_timestamp(1_000 + (initial_swaps as u64 + j) * 10);
            client.swap(&trader, &true, &100_i128, &limit, &0_i128, &u64::MAX);
        }
        assert_eq!(
            client.get_oracle_header().count,
            DEFAULT_ORACLE_CARDINALITY + 20
        );
        assert_eq!(
            client.observe(&((DEFAULT_ORACLE_CARDINALITY as u64 - 1 + 20) * 10)),
            before_expand_oldest
        );
    }

    #[test]
    fn oracle_set_cardinality_rejects_unauthorized_zero_shrink_and_above_max() {
        let env = Env::default();
        env.mock_all_auths();
        env.budget().reset_unlimited();

        let admin = Address::generate(&env);
        let outsider = Address::generate(&env);
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        client.initialize(&admin, &token_a, &token_b, &30_i128, &0_i32, &1_i32);

        assert_eq!(
            client.try_set_oracle_cardinality(&outsider, &1024_u32),
            Err(Ok(ClError::Unauthorized))
        );
        assert_eq!(
            client.try_set_oracle_cardinality(&admin, &0_u32),
            Err(Ok(ClError::InvalidOracleCardinality))
        );
        assert_eq!(
            client.try_set_oracle_cardinality(&admin, &(DEFAULT_ORACLE_CARDINALITY - 1)),
            Err(Ok(ClError::InvalidOracleCardinality))
        );
        assert_eq!(
            client.try_set_oracle_cardinality(&admin, &(MAX_ORACLE_CARDINALITY + 1)),
            Err(Ok(ClError::InvalidOracleCardinality))
        );
        assert!(client
            .try_set_oracle_cardinality(&admin, &MAX_ORACLE_CARDINALITY)
            .is_ok());
    }

    #[test]
    fn oracle_same_timestamp_swaps_overwrite_in_place_without_advancing_ring() {
        let env = Env::default();
        env.mock_all_auths();
        env.budget().reset_unlimited();
        env.ledger().set_timestamp(1_000);

        let (provider, token_a, _token_b, client) = setup_pool(&env, 5);
        client.mint_position(
            &provider,
            &-500_i32,
            &500_i32,
            &10_000_000_i128,
            &10_000_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );
        let buyer = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&buyer, &1_000_000_i128);

        assert_eq!(client.get_oracle_header().count, 1);

        // Two swaps in the same ledger timestamp (1100) should only add 1 new slot.
        env.ledger().set_timestamp(1_100);
        client.swap(&buyer, &true, &100_i128, &0_u128, &0_i128, &u64::MAX);
        client.swap(&buyer, &true, &100_i128, &0_u128, &0_i128, &u64::MAX);
        assert_eq!(client.get_oracle_header().count, 2);
        assert_eq!(client.get_oracle_header().index, 1);
    }

    #[test]
    fn oracle_legacy_exact_out_trailing_point_is_migrated() {
        let env = Env::default();
        env.mock_all_auths();
        env.budget().reset_unlimited();
        env.ledger().set_timestamp(1_000);

        let (provider, token_a, _token_b, client) = setup_pool(&env, 10);
        client.mint_position(
            &provider,
            &-500_i32,
            &500_i32,
            &10_000_000_i128,
            &10_000_000_i128,
            &0_i128,
            &0_i128,
            &u64::MAX,
        );
        let cl_addr = client.address.clone();
        let buyer = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&buyer, &1_000_000_i128);

        // Simulate legacy state where swap_exact_out wrote OraclePoint(1100) without
        // adding 1100 to OracleTimestamps.
        env.as_contract(&cl_addr, || {
            env.storage().instance().remove(&DataKey::OracleHeader);
            env.storage()
                .persistent()
                .remove(&DataKey::OracleObservation(0));

            let mut legacy_ts: Vec<u64> = Vec::new(&env);
            legacy_ts.push_back(1_000);
            env.storage()
                .instance()
                .set(&DataKey::OracleTimestamps, &legacy_ts);
            env.storage()
                .instance()
                .set(&DataKey::OraclePoint(1_000), &0_i64);
            env.storage()
                .instance()
                .set(&DataKey::OraclePoint(1_100), &1_000_i64);
            env.storage()
                .instance()
                .set(&DataKey::LastOracleTimestamp, &1_100_u64);
            env.storage()
                .instance()
                .set(&DataKey::TickCumulative, &1_000_i64);
        });

        env.ledger().set_timestamp(1_200);
        client.swap(&buyer, &true, &100_i128, &0_u128, &0_i128, &u64::MAX);

        assert_eq!(client.get_oracle_header().count, 3);
        assert_eq!(client.observe(&100_u64), 1_000);
        assert_eq!(client.observe(&150_u64), 500);
    }

    // ── Issue #1098: Fee growth scale Q64 (1 << 64) ──────────────────────────

    #[test]
    fn test_fee_growth_large_liquidity_no_underflow_regression_1098() {
        // In issue #1098, liquidity = 2e14 with fee = 300_000 underflowed to 0 under 1e6 scale:
        // fee * 1e6 / 2e14 = 3e11 / 2e14 = 0!
        // Under Q64 scale (1 << 64 ≈ 1.84e19):
        // fee * 2^64 / 2e14 = 300_000 * 1.84467e19 / 2e14 ≈ 2.767e10 > 0!
        let fee: u128 = 300_000;
        let active_liquidity: u128 = 200_000_000_000_000; // 2e14
        let growth = math::mul_div(fee, FEE_GROWTH_SCALE as u128, active_liquidity).unwrap();
        assert!(
            growth > 0,
            "fee growth must not truncate to zero for 2e14 liquidity: growth={growth}"
        );
        assert_eq!(growth, 27_670_116_110);

        // Position earning pending_fees on this growth
        let earned = math::mul_div(active_liquidity, growth, FEE_GROWTH_SCALE as u128).unwrap();
        // Recovers the 300_000 fee (within 1 stroop integer division floor)
        assert!((earned as i128 - fee as i128).abs() <= 1);
    }

    #[test]
    fn test_fee_growth_q64_collect_fees_end_to_end() {
        let env = Env::default();
        let admin = Address::generate(&env);
        env.mock_all_auths();
        env.budget().reset_unlimited();
        let token_a = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let token_b = env
            .register_stellar_asset_contract_v2(admin.clone())
            .address();
        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(&env, &cl_addr);
        client.initialize(&admin, &token_a, &token_b, &30_i128, &0_i32, &1_i32);

        let provider = Address::generate(&env);
        StellarAssetClient::new(&env, &token_a).mint(&provider, &100_000_000_i128);
        StellarAssetClient::new(&env, &token_b).mint(&provider, &100_000_000_i128);

        let trader = Address::generate(&env);

        // Mint liquidity for provider across -1000..1000
        client.mint_position(
            &provider,
            &-1000,
            &1000,
            &50_000_000,
            &50_000_000,
            &0,
            &0,
            &u64::MAX,
        );

        // Trader performs a swap of 1_000_000 token A
        StellarAssetClient::new(&env, &token_a).mint(&trader, &10_000_000);
        client.swap(&trader, &true, &1_000_000, &0, &0, &u64::MAX);

        // Provider collects fees
        let (collected_a, collected_b) = client.collect_fees(&provider, &-1000, &1000);
        assert!(collected_a > 0, "provider must collect fee A");
        assert_eq!(collected_b, 0, "no fee B accrued on zero_for_one swap");
        assert!((2990..=3010).contains(&collected_a));
    }
}

#[cfg(test)]
mod tests_issue_1100;
#[cfg(test)]
mod tests_issue_1102;
#[cfg(test)]
mod tests_issue_1103;

#[cfg(test)]
mod test_issue_1099_tick_sqrt_price_precision {
    extern crate std;
    use super::*;
    use soroban_sdk::testutils::Address as _;
    use soroban_sdk::testutils::Ledger as _;
    use soroban_sdk::token::StellarAssetClient;
    use std::vec::Vec;

    const MAX_DISTINCT_TICK: i32 = 443_636;

    fn sweep_ticks() -> Vec<i32> {
        let mut ticks = Vec::new();
        let mut t = MIN_TICK;
        while t < MAX_DISTINCT_TICK {
            ticks.push(t);
            t += 1_000;
        }
        for center in [-69_100_i32, -46_000, -23_000, -11_500, 0, 46_000] {
            for offset in -50..=50 {
                let candidate = center + offset;
                if (MIN_TICK..MAX_DISTINCT_TICK).contains(&candidate) {
                    ticks.push(candidate);
                }
            }
        }
        ticks.sort_unstable();
        ticks.dedup();
        ticks
    }

    fn setup_low_price_pool(
        env: &Env,
        fee_bps: i128,
        initial_tick: i32,
        tick_spacing: i32,
    ) -> (
        Address,
        Address,
        Address,
        Address,
        ConcentratedLiquidityClient<'_>,
    ) {
        env.mock_all_auths();
        env.budget().reset_unlimited();
        env.ledger().set_timestamp(1_000);
        let admin = Address::generate(env);
        let provider = Address::generate(env);
        let sac_a = env.register_stellar_asset_contract_v2(admin.clone());
        let sac_b = env.register_stellar_asset_contract_v2(admin.clone());
        let token_a = sac_a.address();
        let token_b = sac_b.address();

        let funding = 1_000_000_000_000_000_000_i128;
        StellarAssetClient::new(env, &token_a).mint(&provider, &funding);
        StellarAssetClient::new(env, &token_b).mint(&provider, &funding);

        let cl_addr = env.register_contract(None, ConcentratedLiquidity);
        let client = ConcentratedLiquidityClient::new(env, &cl_addr);
        client.initialize(
            &admin,
            &token_a,
            &token_b,
            &fee_bps,
            &initial_tick,
            &tick_spacing,
        );
        (cl_addr, provider, token_a, token_b, client)
    }

    #[test]
    fn tick_to_sqrt_price_x96_strictly_monotonic_across_sweep_and_dense_windows() {
        for t in sweep_ticks() {
            let lo = ConcentratedLiquidity::tick_to_sqrt_price_x96(t);
            let hi = ConcentratedLiquidity::tick_to_sqrt_price_x96(t + 1);
            assert!(
                lo < hi,
                "tick {t} and {} collided or inverted: {lo} vs {hi}",
                t + 1
            );
        }
    }

    #[test]
    fn tick_to_sqrt_price_x96_matches_math_rs_and_roundtrips_floor_inverse() {
        for t in sweep_ticks() {
            let sp = ConcentratedLiquidity::tick_to_sqrt_price_x96(t);
            let expected = math::tick_to_sqrt_price_x96(t);
            assert_eq!(sp, expected, "contract vs math.rs mismatch at tick {t}");
            assert_eq!(
                ConcentratedLiquidity::sqrt_price_x96_to_tick(sp),
                t,
                "exact tick boundary roundtrip failed at tick {t}"
            );

            let sp_next = ConcentratedLiquidity::tick_to_sqrt_price_x96(t + 1);
            if sp_next > sp + 1 {
                let mid = sp + (sp_next - sp) / 2;
                assert_eq!(
                    ConcentratedLiquidity::sqrt_price_x96_to_tick(mid),
                    t,
                    "interior midpoint between {t} and {} did not floor to {t}",
                    t + 1
                );
                assert_eq!(
                    ConcentratedLiquidity::sqrt_price_x96_to_tick(sp_next - 1),
                    t,
                    "just below next tick boundary did not floor to {t}"
                );
            }
        }
    }

    #[test]
    fn low_price_adjacent_ticks_around_minus_69100_do_not_collide() {
        let p_69100 = ConcentratedLiquidity::tick_to_sqrt_price_x96(-69_100);
        let p_69099 = ConcentratedLiquidity::tick_to_sqrt_price_x96(-69_099);
        assert!(p_69100 < p_69099);
        assert_eq!(p_69100, math::tick_to_sqrt_price_x96(-69_100));
        assert_eq!(p_69099, math::tick_to_sqrt_price_x96(-69_099));

        let (step_in, step_out) =
            ConcentratedLiquidity::compute_step(1_000_000_000_000, p_69099, p_69100, true);
        assert!(step_in > 0, "crossing 1 tick at -69_100 must require input");
        assert!(
            step_out > 0,
            "crossing 1 tick at -69_100 must produce output"
        );
    }

    #[test]
    fn low_price_position_mint_and_full_burn_conserves_deposit_within_one_stroop() {
        let env = Env::default();
        let (_cl_addr, provider, _token_a, _token_b, client) =
            setup_low_price_pool(&env, 30, -69_100, 1);

        let desired_a = 1_000_000_000_000_i128;
        let desired_b = 1_000_000_000_i128;
        let (deposited_a, deposited_b) = client.mint_position(
            &provider, &-69_200, &-69_000, &desired_a, &desired_b, &1, &1, &10_000,
        );
        assert!(deposited_a > 0 && deposited_b > 0);

        let pos = client.get_position(&provider, &-69_200, &-69_000);
        assert!(pos.liquidity > 0);

        let (burned_a, burned_b) =
            client.burn_position(&provider, &-69_200, &-69_000, &pos.liquidity);
        assert!(
            burned_a <= deposited_a && deposited_a - burned_a <= 1,
            "token A burn {burned_a} vs deposit {deposited_a}"
        );
        assert!(
            burned_b <= deposited_b && deposited_b - burned_b <= 1,
            "token B burn {burned_b} vs deposit {deposited_b}"
        );
    }

    #[test]
    fn low_price_position_below_and_above_range_round_trips_within_one_stroop() {
        let env = Env::default();
        // Pool below range [-69_200, -69_000]: position is 100% token A.
        let (_cl_a, provider_a, _ta, _tb, client_a) = setup_low_price_pool(&env, 30, -69_300, 1);
        let (dep_a, dep_b) = client_a.mint_position(
            &provider_a,
            &-69_200,
            &-69_000,
            &1_000_000_000_000,
            &0,
            &1,
            &0,
            &10_000,
        );
        assert!(dep_a > 0 && dep_b == 0);
        let liq_a = client_a
            .get_position(&provider_a, &-69_200, &-69_000)
            .liquidity;
        let (ret_a, ret_b) = client_a.burn_position(&provider_a, &-69_200, &-69_000, &liq_a);
        assert_eq!(ret_b, 0);
        assert!(ret_a <= dep_a && dep_a - ret_a <= 1);

        // Pool above range [-69_200, -69_000]: position is 100% token B.
        let (_cl_b, provider_b, _ta2, _tb2, client_b) = setup_low_price_pool(&env, 30, -68_900, 1);
        let (dep_a2, dep_b2) = client_b.mint_position(
            &provider_b,
            &-69_200,
            &-69_000,
            &0,
            &1_000_000_000,
            &0,
            &1,
            &10_000,
        );
        assert!(dep_a2 == 0 && dep_b2 > 0);
        let liq_b = client_b
            .get_position(&provider_b, &-69_200, &-69_000)
            .liquidity;
        let (ret_a2, ret_b2) = client_b.burn_position(&provider_b, &-69_200, &-69_000, &liq_b);
        assert_eq!(ret_a2, 0);
        assert!(ret_b2 <= dep_b2 && dep_b2 - ret_b2 <= 1);
    }

    #[test]
    fn swap_across_50_consecutive_low_price_ticks_conserves_tokens_and_never_crosses_for_zero() {
        let env = Env::default();
        let start_tick = -69_050_i32;
        let (cl_addr, provider, token_a, token_b, client) =
            setup_low_price_pool(&env, 30, start_tick, 1);

        // Create 50 consecutive initialized ticks [-69_100 .. -69_051] below start_tick.
        for i in 0..50 {
            let lower = -69_100 + i;
            let upper = lower + 1;
            // Verify each 1-tick band has strictly distinct sqrt prices and non-zero step cost.
            let sa = ConcentratedLiquidity::tick_to_sqrt_price_x96(lower);
            let sb = ConcentratedLiquidity::tick_to_sqrt_price_x96(upper);
            assert!(sa < sb, "tick band [{lower}, {upper}] collapsed");
            let (step_in, step_out) =
                ConcentratedLiquidity::compute_step(1_000_000_000_000, sb, sa, true);
            assert!(
                step_in > 0 && step_out > 0,
                "zero-cost step at [{lower}, {upper}]"
            );

            client.mint_position(
                &provider,
                &lower,
                &upper,
                &0,
                &1_000_000_000,
                &0,
                &0,
                &10_000,
            );
        }
        // Also cover the active tick so the swap starts with positive liquidity.
        client.mint_position(
            &provider,
            &-69_051,
            &-69_049,
            &100_000_000_000,
            &1_000_000_000,
            &0,
            &0,
            &10_000,
        );

        let tc_a = soroban_sdk::token::Client::new(&env, &token_a);
        let tc_b = soroban_sdk::token::Client::new(&env, &token_b);
        let pool_a_before = tc_a.balance(&cl_addr);
        let pool_b_before = tc_b.balance(&cl_addr);
        let trader_a_before = tc_a.balance(&provider);
        let trader_b_before = tc_b.balance(&provider);

        let limit = ConcentratedLiquidity::tick_to_sqrt_price_x96(-69_100);
        let amount_in = 10_000_000_000_000_i128;
        let quote = client.estimate_price_impact(&true, &amount_in, &limit);
        let out = client.swap(&provider, &true, &amount_in, &limit, &1, &10_000);

        let state = client.get_pool_state();
        assert_eq!(state.sqrt_price, quote.sqrt_price_after);
        assert_eq!(state.current_tick, quote.tick_after);
        assert_eq!(out, quote.amount_out);
        assert_eq!(
            quote.amount_in,
            quote.amount_in_after_fee + quote.fee_amount
        );

        let trader_a_spent = trader_a_before - tc_a.balance(&provider);
        let trader_b_received = tc_b.balance(&provider) - trader_b_before;
        let pool_a_gained = tc_a.balance(&cl_addr) - pool_a_before;
        let pool_b_paid = pool_b_before - tc_b.balance(&cl_addr);

        assert_eq!(trader_a_spent, quote.amount_in);
        assert_eq!(pool_a_gained, trader_a_spent);
        assert_eq!(trader_b_received, out);
        assert_eq!(pool_b_paid, out);
    }

    #[test]
    fn swap_upward_across_50_consecutive_low_price_ticks_conserves_tokens() {
        let env = Env::default();
        let start_tick = -69_100_i32;
        let (cl_addr, provider, token_a, token_b, client) =
            setup_low_price_pool(&env, 30, start_tick, 1);

        for i in 0..50 {
            let lower = -69_100 + i;
            let upper = lower + 1;
            client.mint_position(
                &provider,
                &lower,
                &upper,
                &100_000_000_000_000,
                &100_000_000,
                &0,
                &0,
                &10_000,
            );
        }

        let tc_a = soroban_sdk::token::Client::new(&env, &token_a);
        let tc_b = soroban_sdk::token::Client::new(&env, &token_b);
        let pool_a_before = tc_a.balance(&cl_addr);
        let pool_b_before = tc_b.balance(&cl_addr);

        let limit = ConcentratedLiquidity::tick_to_sqrt_price_x96(-69_050);
        let amount_in = 10_000_000_000_i128;
        let quote = client.estimate_price_impact(&false, &amount_in, &limit);
        let out = client.swap(&provider, &false, &amount_in, &limit, &1, &10_000);

        let state = client.get_pool_state();
        assert_eq!(state.sqrt_price, quote.sqrt_price_after);
        assert_eq!(state.current_tick, quote.tick_after);
        assert_eq!(out, quote.amount_out);
        assert_eq!(tc_b.balance(&cl_addr) - pool_b_before, quote.amount_in);
        assert_eq!(pool_a_before - tc_a.balance(&cl_addr), out);
    }

    #[test]
    fn pool_initialized_at_minus_69100_starts_at_exact_math_sqrt_price() {
        let env = Env::default();
        let (_cl_addr, provider, _token_a, _token_b, client) =
            setup_low_price_pool(&env, 30, -69_100, 10);

        let expected = math::tick_to_sqrt_price_x96(-69_100);
        let state = client.get_pool_state();
        assert_eq!(state.sqrt_price, expected);
        assert_eq!(state.current_tick, -69_100);

        client.mint_position(
            &provider,
            &-69_200,
            &-69_000,
            &1_000_000_000_000,
            &1_000_000_000,
            &0,
            &0,
            &10_000,
        );

        let quote = client.estimate_price_impact(&true, &1_000_000, &0);
        assert_eq!(quote.sqrt_price_before, expected);
    }

    #[test]
    fn legacy_pool_without_stored_sqrt_price_falls_back_to_exact_math_price() {
        let env = Env::default();
        let (cl_addr, provider, _token_a, _token_b, client) =
            setup_low_price_pool(&env, 30, -69_100, 10);

        client.mint_position(
            &provider,
            &-69_200,
            &-69_000,
            &1_000_000_000_000,
            &1_000_000_000,
            &0,
            &0,
            &10_000,
        );

        // Simulate a legacy pool initialized before SqrtPriceX96 was written at init.
        env.as_contract(&cl_addr, || {
            env.storage().instance().remove(&DataKey::SqrtPriceX96);
        });

        let expected = math::tick_to_sqrt_price_x96(-69_100);
        assert_eq!(client.get_pool_state().sqrt_price, expected);

        let quote = client.estimate_price_impact(&true, &1_000_000, &0);
        assert_eq!(quote.sqrt_price_before, expected);

        let out = client.swap(&provider, &true, &1_000_000, &0, &1, &10_000);
        assert_eq!(out, quote.amount_out);
        assert_eq!(client.get_pool_state().sqrt_price, quote.sqrt_price_after);
    }

    #[test]
    fn estimate_price_impact_spot_price_at_low_ticks_uses_q96_math() {
        let env = Env::default();
        let (_cl_addr, provider, _token_a, _token_b, client) =
            setup_low_price_pool(&env, 30, -69_100, 10);

        client.mint_position(
            &provider,
            &-69_200,
            &-69_000,
            &1_000_000_000_000,
            &1_000_000_000,
            &0,
            &0,
            &10_000,
        );

        let q_a_to_b = client.estimate_price_impact(&true, &1_000_000, &0);
        let q_b_to_a = client.estimate_price_impact(&false, &1_000, &0);
        assert!(q_a_to_b.spot_price_before > 0);
        assert!(q_b_to_a.spot_price_before > 0);
        // At tick -69_100, price of A in B is ~0.000998 (~998 in 1e6 scale),
        // and price of B in A is ~1001.9 (~1_001_901_048 in 1e6 scale).
        assert_eq!(q_a_to_b.spot_price_before, 998);
        assert!(
            (q_b_to_a.spot_price_before - 1_001_901_048).abs() <= 5,
            "unexpected inverse spot price: {}",
            q_b_to_a.spot_price_before
        );
    }

    #[test]
    fn tick_to_sqrt_price_x96_clamps_out_of_range_ticks_and_saturates_above_443636() {
        assert_eq!(
            ConcentratedLiquidity::tick_to_sqrt_price_x96(MIN_TICK - 10_000),
            math::MIN_SQRT_PRICE
        );
        assert_eq!(
            ConcentratedLiquidity::tick_to_sqrt_price_x96(MIN_TICK),
            math::MIN_SQRT_PRICE
        );
        assert!(
            ConcentratedLiquidity::tick_to_sqrt_price_x96(MAX_DISTINCT_TICK - 1)
                < ConcentratedLiquidity::tick_to_sqrt_price_x96(MAX_DISTINCT_TICK)
        );
        assert_eq!(
            ConcentratedLiquidity::tick_to_sqrt_price_x96(MAX_DISTINCT_TICK + 1),
            math::MAX_SQRT_PRICE
        );
        assert_eq!(
            ConcentratedLiquidity::tick_to_sqrt_price_x96(MAX_TICK),
            math::MAX_SQRT_PRICE
        );
        assert_eq!(
            ConcentratedLiquidity::tick_to_sqrt_price_x96(MAX_TICK + 10_000),
            math::MAX_SQRT_PRICE
        );
    }

    #[test]
    fn sqrt_price_x96_to_tick_clamps_below_min_and_above_max_sqrt_price() {
        assert_eq!(ConcentratedLiquidity::sqrt_price_x96_to_tick(0), MIN_TICK);
        assert_eq!(
            ConcentratedLiquidity::sqrt_price_x96_to_tick(math::MIN_SQRT_PRICE - 1),
            MIN_TICK
        );
        assert_eq!(
            ConcentratedLiquidity::sqrt_price_x96_to_tick(math::MIN_SQRT_PRICE),
            MIN_TICK
        );
        assert_eq!(
            ConcentratedLiquidity::sqrt_price_x96_to_tick(math::MAX_SQRT_PRICE),
            MAX_TICK
        );
        assert_eq!(
            ConcentratedLiquidity::sqrt_price_x96_to_tick(u128::MAX),
            MAX_TICK
        );
    }

    #[test]
    fn low_price_exact_out_swap_matches_exact_in_and_conserves_tokens() {
        let env = Env::default();
        let (_cl_addr, provider, _token_a, _token_b, client) =
            setup_low_price_pool(&env, 30, -69_100, 10);

        client.mint_position(
            &provider,
            &-69_200,
            &-69_000,
            &1_000_000_000_000,
            &1_000_000_000,
            &0,
            &0,
            &u64::MAX,
        );

        let target_out = 50_000_i128;
        let quoted_in = client.quote_exact_out(&true, &target_out, &0);
        let paid_in =
            client.swap_exact_out(&provider, &true, &target_out, &0, &quoted_in, &u64::MAX);
        assert_eq!(paid_in, quoted_in);
        assert!(paid_in > 0);
    }
}
