//! V2 → V3 AMM Migration Contract (Issues #266, #1150)
//!
//! Atomically migrates a liquidity position from a V2 constant-product pool
//! to a V3 concentrated-liquidity pool in a single transaction.
//!
//! One deployment serves every pool pair. The pools are chosen per call and
//! are attested by the factory configured at `initialize`, so a caller cannot
//! route funds through an arbitrary contract.
//!
//! Flow:
//!   1. LP authorizes the migration call and its nested pool calls.
//!   2. LP calls `migrate` with the V2 pool, the CL pool, their V2 LP share
//!      amount and desired V3 range.
//!   3. The contract checks both pools against the factory registry.
//!   4. The V2 pool burns the shares and returns token_a + token_b to the LP.
//!   5. The CL pool pulls only the amounts needed for the selected range
//!      directly from the LP and records the position under the LP's address.
//!   6. Any leftover tokens remain in the LP's wallet.
//!
//! Instances deployed before #1150 were bound to a single pair. After an
//! `upgrade` they keep working through `migrate_default` /
//! `preview_range_default`, which read the stored pair. The admin calls
//! `set_factory` once to unlock the multi-pair `migrate`.

#![no_std]

use pool_interfaces::{ConcentratedLiquidityClient, FactoryClient};
use soroban_amm_sdk::emit_versioned_event;
use soroban_sdk::{
    contract, contractclient, contracterror, contractimpl, contracttype, Address, BytesN, Env,
    Symbol, U256,
};

// ── Errors ────────────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum MigrationError {
    NotInitialized = 1,
    AlreadyInitialized = 2,
    Unauthorized = 3,
    ZeroShares = 4,
    InvalidRange = 5,
    SlippageExceeded = 6,
    MigrationFailed = 7,
    TokenMismatch = 8,
    /// `migrate`/`migrate_default` called while the contract is paused.
    Paused = 9,
    /// A pool address is not attested by the factory registry.
    UnknownPool = 10,
    /// `accept_admin` called without a prior `propose_admin`.
    NoPendingAdmin = 11,
    /// `accept_admin` called by an address other than the nominee.
    WrongAdmin = 12,
    /// A factory-validated entry point was called before a factory was set.
    FactoryNotSet = 13,
    /// A `*_default` entry point was called but no default pair is stored.
    NoDefaultPair = 14,
    /// `preview_migration` could not quote the position on the CL pool.
    PreviewFailed = 15,
}

// ── Storage keys ─────────────────────────────────────────────────────────────

#[contracttype]
pub enum DataKey {
    Admin,
    /// Default V2 pool for `migrate_default`.
    V2Pool,
    /// Default V3 pool for `migrate_default`.
    V3Pool,
    Factory,
    PendingAdmin,
    Paused,
}

const MIN_TTL: u32 = 172_800;
const BUMP_TO: u32 = 518_400;
const MIN_TICK: i32 = -887_272;
const MAX_TICK: i32 = 887_272;

/// Reference liquidity used to price one unit of liquidity on a tick range
/// through the CL pool's own `quote_position`.
const QUOTE_LIQUIDITY: u128 = 1_000_000_000_000_000_000;
/// Bound on the rounding-correction loop in `preview_migration`.
const MAX_PREVIEW_ADJUSTMENTS: u32 = 8;

// ── External interfaces ───────────────────────────────────────────────────────

/// Minimal V2 AMM interface needed for migration.
#[contractclient(name = "V2PoolClient")]
pub trait V2PoolInterface {
    fn remove_liquidity(
        env: Env,
        provider: Address,
        shares: i128,
        min_a: i128,
        min_b: i128,
        deadline: u64,
    ) -> Result<(i128, i128), soroban_sdk::Error>;

    fn get_info(env: Env) -> V2PoolInfo;
}

#[contracttype]
#[derive(Clone)]
pub struct V2PoolInfo {
    pub token_a: Address,
    pub token_b: Address,
    pub reserve_a: i128,
    pub reserve_b: i128,
    pub total_shares: i128,
    pub fee_bps: i128,
    pub flash_loan_fee_bps: i128,
    pub admin: Address,
    pub fee_recipient: Address,
    pub protocol_fee_bps: i128,
    pub lp_rebate_bps: i128,
}

/// Minimal CL read interface for `preview_migration`.
#[contractclient(name = "ClQuoteClient")]
pub trait ClQuoteInterface {
    fn quote_position(
        env: Env,
        lower_tick: i32,
        upper_tick: i32,
        liquidity: i128,
    ) -> Result<(i128, i128), pool_interfaces::ClError>;
}

// ── Params / results ──────────────────────────────────────────────────────────

/// Arguments of [`MigrationContract::migrate`] other than the caller and the
/// two pools. Bundled because a Soroban function takes at most 10 arguments.
#[contracttype]
#[derive(Clone, Debug)]
pub struct MigrateParams {
    /// Number of V2 LP tokens to burn.
    pub v2_shares: i128,
    /// Minimum V2 token_a to receive from the withdrawal.
    pub min_a: i128,
    /// Minimum V2 token_b to receive from the withdrawal.
    pub min_b: i128,
    /// Desired lower tick; `i32::MIN` auto-computes it.
    pub tick_lower: i32,
    /// Desired upper tick; `i32::MAX` auto-computes it.
    pub tick_upper: i32,
    /// Half-width of an auto-computed bound (ignored for explicit ticks).
    pub range_width_ticks: i32,
    /// Minimum V3 token_a deposited.
    pub min_deposit_a: i128,
    /// Minimum V3 token_b deposited.
    pub min_deposit_b: i128,
    /// Latest ledger timestamp at which the call is valid.
    pub deadline: u64,
}

#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct MigrationResult {
    /// Amount of the V3 pool's token_a deposited into the CL position.
    pub deposited_a: i128,
    /// Amount of the V3 pool's token_b deposited into the CL position.
    pub deposited_b: i128,
    /// Amount of the V3 pool's token_a that remained with the LP.
    pub leftover_a: i128,
    /// Amount of the V3 pool's token_b that remained with the LP.
    pub leftover_b: i128,
    /// Final lower tick used for the V3 range.
    pub tick_lower: i32,
    /// Final upper tick used for the V3 range.
    pub tick_upper: i32,
    /// Receipt NFT minted by the CL pool, if position NFTs are configured.
    pub position_token_id: Option<u64>,
}

/// Read-only estimate of what [`MigrationContract::migrate`] would do.
/// All amounts are in the CL pool's canonical token order.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct MigrationPreview {
    /// V3 token_a withdrawn from the V2 pool for the shares.
    pub withdraw_a: i128,
    /// V3 token_b withdrawn from the V2 pool for the shares.
    pub withdraw_b: i128,
    /// V3 token_a that would be deposited into the CL position.
    pub deposit_a: i128,
    /// V3 token_b that would be deposited into the CL position.
    pub deposit_b: i128,
    /// Liquidity the CL position would receive.
    pub liquidity: i128,
    /// V3 token_a left in the LP's wallet.
    pub leftover_a: i128,
    /// V3 token_b left in the LP's wallet.
    pub leftover_b: i128,
    pub tick_lower: i32,
    pub tick_upper: i32,
}

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct MigrationContract;

fn extend_ttl(env: &Env) {
    env.storage().instance().extend_ttl(MIN_TTL, BUMP_TO);
}

#[contractimpl]
impl MigrationContract {
    // ── Setup ─────────────────────────────────────────────────────────────────

    /// Initialize the migration helper.
    ///
    /// # Parameters
    /// - `admin`   – Contract administrator (pause, upgrade, admin rotation).
    /// - `factory` – Factory whose registry attests every pool passed to
    ///               `migrate` and `preview_migration`.
    pub fn initialize(env: Env, admin: Address, factory: Address) -> Result<(), MigrationError> {
        extend_ttl(&env);
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(MigrationError::AlreadyInitialized);
        }
        admin.require_auth();

        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage().instance().set(&DataKey::Factory, &factory);
        Ok(())
    }

    // ── Admin ─────────────────────────────────────────────────────────────────

    /// Point the contract at a factory. Needed once on instances deployed
    /// before #1150, which were initialized without one. Admin-only.
    pub fn set_factory(env: Env, factory: Address) -> Result<(), MigrationError> {
        let admin = Self::require_admin(&env)?;
        admin.require_auth();
        env.storage().instance().set(&DataKey::Factory, &factory);
        emit_versioned_event!(env, (Symbol::new(&env, "factory_set"),), (factory,));
        Ok(())
    }

    /// Store the pair used by `migrate_default` / `preview_range_default`.
    /// Both pools must be attested by the factory. Admin-only.
    pub fn set_default_pair(
        env: Env,
        v2_pool: Address,
        v3_pool: Address,
    ) -> Result<(), MigrationError> {
        let admin = Self::require_admin(&env)?;
        admin.require_auth();
        Self::validate_pools(&env, &v2_pool, &v3_pool)?;
        env.storage().instance().set(&DataKey::V2Pool, &v2_pool);
        env.storage().instance().set(&DataKey::V3Pool, &v3_pool);
        emit_versioned_event!(
            env,
            (Symbol::new(&env, "default_pair_set"),),
            (v2_pool, v3_pool)
        );
        Ok(())
    }

    /// Block `migrate` / `migrate_default`. Previews stay available. Admin-only.
    pub fn pause(env: Env) -> Result<(), MigrationError> {
        let admin = Self::require_admin(&env)?;
        admin.require_auth();
        env.storage().instance().set(&DataKey::Paused, &true);
        emit_versioned_event!(env, (Symbol::new(&env, "paused"),), (admin,));
        Ok(())
    }

    /// Re-enable migrations. Admin-only.
    pub fn unpause(env: Env) -> Result<(), MigrationError> {
        let admin = Self::require_admin(&env)?;
        admin.require_auth();
        env.storage().instance().set(&DataKey::Paused, &false);
        emit_versioned_event!(env, (Symbol::new(&env, "unpaused"),), (admin,));
        Ok(())
    }

    /// Replace the contract WASM. State is preserved. Admin-only.
    pub fn upgrade(env: Env, new_wasm_hash: BytesN<32>) -> Result<(), MigrationError> {
        let admin = Self::require_admin(&env)?;
        admin.require_auth();
        env.deployer()
            .update_current_contract_wasm(new_wasm_hash.clone());
        emit_versioned_event!(env, (Symbol::new(&env, "upgraded"),), (new_wasm_hash,));
        Ok(())
    }

    /// Nominate a new admin. The nominee must call `accept_admin`.
    /// Admin-only.
    pub fn propose_admin(env: Env, new_admin: Address) -> Result<(), MigrationError> {
        let admin = Self::require_admin(&env)?;
        admin.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::PendingAdmin, &new_admin);
        emit_versioned_event!(
            env,
            (Symbol::new(&env, "admin_nominated"),),
            (admin, new_admin)
        );
        Ok(())
    }

    /// Accept a pending admin nomination. Only the nominee can call this.
    pub fn accept_admin(env: Env, new_admin: Address) -> Result<(), MigrationError> {
        extend_ttl(&env);
        let pending: Address = env
            .storage()
            .instance()
            .get(&DataKey::PendingAdmin)
            .ok_or(MigrationError::NoPendingAdmin)?;
        if new_admin != pending {
            return Err(MigrationError::WrongAdmin);
        }
        new_admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &new_admin);
        env.storage().instance().remove(&DataKey::PendingAdmin);
        emit_versioned_event!(env, (Symbol::new(&env, "admin_changed"),), (new_admin,));
        Ok(())
    }

    // ── Migration ─────────────────────────────────────────────────────────────

    /// Migrate a V2 LP position to V3 in a single atomic transaction.
    ///
    /// Both pools must be registered with the factory (`UnknownPool`
    /// otherwise) and trade the same pair (`TokenMismatch`).
    ///
    /// # Parameters
    /// - `provider` – LP address; must authorise this call.
    /// - `v2_pool`  – Constant-product pool holding the LP's shares.
    /// - `v3_pool`  – Concentrated-liquidity pool receiving the position.
    /// - `params`   – Shares, slippage guards, range and deadline; see
    ///                [`MigrateParams`]. `tick_lower = i32::MIN` /
    ///                `tick_upper = i32::MAX` auto-compute that bound.
    ///
    /// # Returns
    /// A [`MigrationResult`] describing what was deposited and left with the LP,
    /// the final range, and the optional V3 position NFT ID.
    pub fn migrate(
        env: Env,
        provider: Address,
        v2_pool: Address,
        v3_pool: Address,
        params: MigrateParams,
    ) -> Result<MigrationResult, MigrationError> {
        extend_ttl(&env);
        Self::require_initialized(&env)?;
        Self::require_not_paused(&env)?;
        if params.v2_shares <= 0 {
            return Err(MigrationError::ZeroShares);
        }
        provider.require_auth();
        Self::validate_pools(&env, &v2_pool, &v3_pool)?;
        Self::execute(&env, &provider, &v2_pool, &v3_pool, &params)
    }

    /// Single-pair entry point kept for instances deployed before #1150.
    /// Migrates through the pair stored by `set_default_pair` (or by the old
    /// three-argument `initialize` on a pre-#1150 deployment).
    #[allow(clippy::too_many_arguments)]
    pub fn migrate_default(
        env: Env,
        provider: Address,
        v2_shares: i128,
        min_a: i128,
        min_b: i128,
        tick_lower: i32,
        tick_upper: i32,
        range_width_ticks: i32,
        min_deposit_a: i128,
        min_deposit_b: i128,
        deadline: u64,
    ) -> Result<MigrationResult, MigrationError> {
        extend_ttl(&env);
        Self::require_initialized(&env)?;
        Self::require_not_paused(&env)?;
        if v2_shares <= 0 {
            return Err(MigrationError::ZeroShares);
        }
        provider.require_auth();
        let (v2_pool, v3_pool) = Self::default_pair(&env)?;
        let params = MigrateParams {
            v2_shares,
            min_a,
            min_b,
            tick_lower,
            tick_upper,
            range_width_ticks,
            min_deposit_a,
            min_deposit_b,
            deadline,
        };
        Self::execute(&env, &provider, &v2_pool, &v3_pool, &params)
    }

    // ── Read-only helpers ─────────────────────────────────────────────────────

    /// Preview what `migrate` would do for `shares` of `v2_pool` into
    /// `v3_pool`, without moving funds. Works while paused.
    ///
    /// The V2 withdrawal is computed from the pool's reserves exactly as
    /// `remove_liquidity` does. The CL deposit is priced with the CL pool's
    /// own `quote_position`: the largest liquidity whose quoted amounts fit
    /// inside the withdrawal, so `deposit_*` matches the amounts `mint_position`
    /// takes to within rounding (1 unit on realistic sizes).
    ///
    /// `tick_lower` / `tick_upper` follow the same sentinel rules as `migrate`.
    pub fn preview_migration(
        env: Env,
        v2_pool: Address,
        v3_pool: Address,
        shares: i128,
        tick_lower: i32,
        tick_upper: i32,
        range_width_ticks: i32,
    ) -> Result<MigrationPreview, MigrationError> {
        extend_ttl(&env);
        Self::require_initialized(&env)?;
        if shares <= 0 {
            return Err(MigrationError::ZeroShares);
        }
        let reversed = Self::validate_pools(&env, &v2_pool, &v3_pool)?;

        let info = V2PoolClient::new(&env, &v2_pool).get_info();
        if info.total_shares <= 0 || shares > info.total_shares {
            return Err(MigrationError::ZeroShares);
        }
        let out_a = shares
            .checked_mul(info.reserve_a)
            .ok_or(MigrationError::PreviewFailed)?
            / info.total_shares;
        let out_b = shares
            .checked_mul(info.reserve_b)
            .ok_or(MigrationError::PreviewFailed)?
            / info.total_shares;
        let (withdraw_a, withdraw_b) = if reversed {
            (out_b, out_a)
        } else {
            (out_a, out_b)
        };

        let v3_client = ConcentratedLiquidityClient::new(&env, &v3_pool);
        let (lower, upper) =
            Self::compute_range(&v3_client, tick_lower, tick_upper, range_width_ticks)?;

        let quote = ClQuoteClient::new(&env, &v3_pool);
        let (liquidity, deposit_a, deposit_b) =
            Self::size_position(&env, &quote, lower, upper, withdraw_a, withdraw_b)?;

        Ok(MigrationPreview {
            withdraw_a,
            withdraw_b,
            deposit_a,
            deposit_b,
            liquidity,
            leftover_a: withdraw_a - deposit_a,
            leftover_b: withdraw_b - deposit_b,
            tick_lower: lower,
            tick_upper: upper,
        })
    }

    /// Preview the final V3 tick range for `v3_pool` without executing.
    ///
    /// Useful for off-chain UIs to show the user what range they'll get before
    /// they sign the migration transaction.
    pub fn preview_range(
        env: Env,
        v3_pool: Address,
        tick_lower: i32,
        tick_upper: i32,
        range_width_ticks: i32,
    ) -> Result<(i32, i32), MigrationError> {
        extend_ttl(&env);
        Self::require_initialized(&env)?;
        let factory = Self::factory(&env)?;
        if !FactoryClient::new(&env, &factory).is_cl_pool(&v3_pool) {
            return Err(MigrationError::UnknownPool);
        }
        let v3_client = ConcentratedLiquidityClient::new(&env, &v3_pool);
        Self::compute_range(&v3_client, tick_lower, tick_upper, range_width_ticks)
    }

    /// `preview_range` against the stored default pair.
    pub fn preview_range_default(
        env: Env,
        tick_lower: i32,
        tick_upper: i32,
        range_width_ticks: i32,
    ) -> Result<(i32, i32), MigrationError> {
        extend_ttl(&env);
        Self::require_initialized(&env)?;
        let (_, v3_pool) = Self::default_pair(&env)?;
        let v3_client = ConcentratedLiquidityClient::new(&env, &v3_pool);
        Self::compute_range(&v3_client, tick_lower, tick_upper, range_width_ticks)
    }

    pub fn get_admin(env: Env) -> Option<Address> {
        extend_ttl(&env);
        env.storage().instance().get(&DataKey::Admin)
    }

    pub fn get_pending_admin(env: Env) -> Option<Address> {
        extend_ttl(&env);
        env.storage().instance().get(&DataKey::PendingAdmin)
    }

    pub fn get_factory(env: Env) -> Option<Address> {
        extend_ttl(&env);
        env.storage().instance().get(&DataKey::Factory)
    }

    pub fn get_default_pair(env: Env) -> Option<(Address, Address)> {
        extend_ttl(&env);
        Self::default_pair(&env).ok()
    }

    pub fn is_paused(env: Env) -> bool {
        extend_ttl(&env);
        env.storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
    }

    // ── Internals ─────────────────────────────────────────────────────────────

    fn require_initialized(env: &Env) -> Result<(), MigrationError> {
        if env.storage().instance().has(&DataKey::Admin) {
            Ok(())
        } else {
            Err(MigrationError::NotInitialized)
        }
    }

    fn require_admin(env: &Env) -> Result<Address, MigrationError> {
        extend_ttl(env);
        env.storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(MigrationError::NotInitialized)
    }

    fn require_not_paused(env: &Env) -> Result<(), MigrationError> {
        if env
            .storage()
            .instance()
            .get(&DataKey::Paused)
            .unwrap_or(false)
        {
            Err(MigrationError::Paused)
        } else {
            Ok(())
        }
    }

    fn factory(env: &Env) -> Result<Address, MigrationError> {
        env.storage()
            .instance()
            .get(&DataKey::Factory)
            .ok_or(MigrationError::FactoryNotSet)
    }

    fn default_pair(env: &Env) -> Result<(Address, Address), MigrationError> {
        let v2: Option<Address> = env.storage().instance().get(&DataKey::V2Pool);
        let v3: Option<Address> = env.storage().instance().get(&DataKey::V3Pool);
        match (v2, v3) {
            (Some(v2), Some(v3)) => Ok((v2, v3)),
            _ => Err(MigrationError::NoDefaultPair),
        }
    }

    /// Check both pools against the factory registry and return whether the
    /// CL pool lists the pair in the opposite order to the V2 pool.
    ///
    /// The factory is consulted before any call to the pool arguments, so an
    /// attacker-deployed contract is never invoked: the V2 pool must have a
    /// token-pair record and be the factory's pool for that pair, and the CL
    /// pool must be a registered CL pool that the factory resolves from its
    /// own pair and fee tier.
    fn validate_pools(
        env: &Env,
        v2_pool: &Address,
        v3_pool: &Address,
    ) -> Result<bool, MigrationError> {
        let factory = FactoryClient::new(env, &Self::factory(env)?);

        if factory.is_cl_pool(v2_pool) || !factory.is_cl_pool(v3_pool) {
            return Err(MigrationError::UnknownPool);
        }
        let (token_a, token_b) = factory
            .get_pool_tokens(v2_pool)
            .ok_or(MigrationError::UnknownPool)?;
        if factory.get_pool(&token_a, &token_b) != Some(v2_pool.clone()) {
            return Err(MigrationError::UnknownPool);
        }

        let v3_client = ConcentratedLiquidityClient::new(env, v3_pool);
        let (v3_token_a, v3_token_b) = v3_client.get_tokens();
        let reversed = if token_a == v3_token_a && token_b == v3_token_b {
            false
        } else if token_a == v3_token_b && token_b == v3_token_a {
            true
        } else {
            return Err(MigrationError::TokenMismatch);
        };

        let fee_bps = v3_client.fee_bps();
        if factory.get_cl_pool(&v3_token_a, &v3_token_b, &fee_bps) != Some(v3_pool.clone()) {
            return Err(MigrationError::UnknownPool);
        }
        Ok(reversed)
    }

    /// Withdraw from the V2 pool and mint into the CL pool.
    fn execute(
        env: &Env,
        provider: &Address,
        v2_pool: &Address,
        v3_pool: &Address,
        params: &MigrateParams,
    ) -> Result<MigrationResult, MigrationError> {
        // Verify V2 and V3 pools trade the same pair.
        let v2_client = V2PoolClient::new(env, v2_pool);
        let pool_info = v2_client.get_info();

        let token_a = pool_info.token_a.clone();
        let token_b = pool_info.token_b.clone();

        let v3_client = ConcentratedLiquidityClient::new(env, v3_pool);
        let (v3_token_a, v3_token_b) = v3_client.get_tokens();

        let reversed = if token_a == v3_token_a && token_b == v3_token_b {
            false
        } else if token_a == v3_token_b && token_b == v3_token_a {
            true
        } else {
            return Err(MigrationError::TokenMismatch);
        };

        // Validate the range before burning shares or moving any funds.
        let (final_tick_lower, final_tick_upper) = Self::compute_range(
            &v3_client,
            params.tick_lower,
            params.tick_upper,
            params.range_width_ticks,
        )?;

        // The V2 pool returns the withdrawn assets directly to the LP.
        let (received_a, received_b) = v2_client.remove_liquidity(
            provider,
            &params.v2_shares,
            &params.min_a,
            &params.min_b,
            &params.deadline,
        );

        // Map the withdrawal into the CL pool's canonical token order.
        let (desired_a, desired_b) = if reversed {
            (received_b, received_a)
        } else {
            (received_a, received_b)
        };

        // Mint under the LP's address. The signed auth tree authorizes the
        // nested V2 withdrawal, CL mint, and token transfers from the LP.
        let (deposited_a, deposited_b) = v3_client.mint_position(
            provider,
            &final_tick_lower,
            &final_tick_upper,
            &desired_a,
            &desired_b,
            &params.min_deposit_a,
            &params.min_deposit_b,
            &params.deadline,
        );

        let leftover_a = desired_a
            .checked_sub(deposited_a)
            .ok_or(MigrationError::MigrationFailed)?;
        let leftover_b = desired_b
            .checked_sub(deposited_b)
            .ok_or(MigrationError::MigrationFailed)?;
        let position_token_id =
            v3_client.position_token_id(provider, &final_tick_lower, &final_tick_upper);

        emit_versioned_event!(
            env,
            (Symbol::new(env, "migrated"), provider.clone()),
            (
                params.v2_shares,
                deposited_a,
                deposited_b,
                position_token_id,
                leftover_a,
                leftover_b,
            )
        );

        Ok(MigrationResult {
            deposited_a,
            deposited_b,
            leftover_a,
            leftover_b,
            tick_lower: final_tick_lower,
            tick_upper: final_tick_upper,
            position_token_id,
        })
    }

    /// Largest liquidity on `[lower, upper)` whose quoted amounts fit inside
    /// `(desired_a, desired_b)`, with the amounts the CL pool quotes for it.
    ///
    /// Amounts are linear in liquidity, so one quote at a large reference
    /// liquidity gives the per-liquidity price of each leg; the final quote at
    /// the chosen liquidity is exact, and a short loop absorbs the rounding of
    /// the reference quote.
    fn size_position(
        env: &Env,
        quote: &ClQuoteClient,
        lower: i32,
        upper: i32,
        desired_a: i128,
        desired_b: i128,
    ) -> Result<(i128, i128, i128), MigrationError> {
        let quote_at = |liquidity: i128| -> Result<(i128, i128), MigrationError> {
            match quote.try_quote_position(&lower, &upper, &liquidity) {
                Ok(Ok(amounts)) => Ok(amounts),
                _ => Err(MigrationError::PreviewFailed),
            }
        };

        let (ref_a, ref_b) = quote_at(QUOTE_LIQUIDITY as i128)?;
        let scale = |desired: i128, reference: i128| -> Result<u128, MigrationError> {
            let value = U256::from_u128(env, desired as u128)
                .mul(&U256::from_u128(env, QUOTE_LIQUIDITY))
                .div(&U256::from_u128(env, reference as u128));
            value.to_u128().ok_or(MigrationError::PreviewFailed)
        };

        let mut liquidity: Option<u128> = None;
        if ref_a > 0 {
            liquidity = Some(scale(desired_a.max(0), ref_a)?);
        }
        if ref_b > 0 {
            let from_b = scale(desired_b.max(0), ref_b)?;
            liquidity = Some(liquidity.map_or(from_b, |l| l.min(from_b)));
        }
        let mut liquidity = i128::try_from(liquidity.ok_or(MigrationError::PreviewFailed)?)
            .map_err(|_| MigrationError::PreviewFailed)?
            .max(1);

        for _ in 0..=MAX_PREVIEW_ADJUSTMENTS {
            let (a, b) = quote_at(liquidity)?;
            if a <= desired_a && b <= desired_b {
                return Ok((liquidity, a, b));
            }
            liquidity -= (liquidity >> 20) + 1;
            if liquidity <= 0 {
                break;
            }
        }
        Err(MigrationError::PreviewFailed)
    }

    /// Compute the final [tick_lower, tick_upper] for the V3 deposit.
    ///
    /// `tick_lower` and `tick_upper` are independently opt-in to
    /// auto-computation, matching `migrate`'s doc comment: passing
    /// `i32::MIN` for `tick_lower` auto-computes only the lower bound, and
    /// passing `i32::MAX` for `tick_upper` auto-computes only the upper
    /// bound. An explicit value for either bound is always kept as-is —
    /// it is never silently discarded, even when the other bound is a
    /// sentinel.
    fn compute_range(
        v3_client: &ConcentratedLiquidityClient,
        tick_lower: i32,
        tick_upper: i32,
        range_width_ticks: i32,
    ) -> Result<(i32, i32), MigrationError> {
        let lower_auto = tick_lower == i32::MIN;
        let upper_auto = tick_upper == i32::MAX;
        let state = v3_client.get_pool_state();
        let tick_spacing = state.tick_spacing;

        if tick_spacing <= 0 {
            return Err(MigrationError::InvalidRange);
        }

        if (!lower_auto && !Self::is_valid_explicit_tick(tick_lower, tick_spacing))
            || (!upper_auto && !Self::is_valid_explicit_tick(tick_upper, tick_spacing))
        {
            return Err(MigrationError::InvalidRange);
        }

        if (lower_auto || upper_auto) && range_width_ticks <= 0 {
            return Err(MigrationError::InvalidRange);
        }

        let spacing = i64::from(tick_spacing);
        let min_usable_tick = Self::align_up(i64::from(MIN_TICK), spacing);
        let max_usable_tick = Self::align_down(i64::from(MAX_TICK), spacing);
        let current_tick = i64::from(state.current_tick);
        let width = i64::from(range_width_ticks);

        // Only compute the bounds that were explicitly marked with sentinels.
        // An explicit lower/upper bound must never be silently overwritten.
        let lower = if lower_auto {
            Self::align_down(current_tick - width, spacing).clamp(min_usable_tick, max_usable_tick)
                as i32
        } else {
            tick_lower
        };

        let upper = if upper_auto {
            Self::align_up(current_tick + width, spacing).clamp(min_usable_tick, max_usable_tick)
                as i32
        } else {
            tick_upper
        };

        if lower >= upper {
            return Err(MigrationError::InvalidRange);
        }

        Ok((lower, upper))
    }

    fn is_valid_explicit_tick(tick: i32, tick_spacing: i32) -> bool {
        (MIN_TICK..=MAX_TICK).contains(&tick) && tick % tick_spacing == 0
    }

    fn align_down(tick: i64, tick_spacing: i64) -> i64 {
        tick.div_euclid(tick_spacing) * tick_spacing
    }

    fn align_up(tick: i64, tick_spacing: i64) -> i64 {
        let quotient = tick.div_euclid(tick_spacing);
        if tick.rem_euclid(tick_spacing) == 0 {
            quotient * tick_spacing
        } else {
            (quotient + 1) * tick_spacing
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use amm::{AmmPool, AmmPoolClient};
    extern crate std;
    use pool_interfaces::PoolState;
    use soroban_sdk::testutils::{Address as _, Events, Ledger as _, MockAuth, MockAuthInvoke};
    use soroban_sdk::token::{StellarAssetClient, TokenClient};
    use soroban_sdk::{IntoVal, String, Symbol, TryFromVal};
    use token::{LpToken, LpTokenClient};

    /// Minimal CL state stub used only by the range helper tests.
    #[contract]
    struct RangePool;

    #[contractimpl]
    impl RangePool {
        pub fn get_pool_state(_env: Env) -> PoolState {
            PoolState {
                sqrt_price: 0,
                current_tick: 1_000,
                active_liquidity: 0,
                tick_spacing: 1,
            }
        }

        pub fn get_tokens(env: Env) -> (Address, Address) {
            (
                env.storage().instance().get(&0u32).unwrap(),
                env.storage().instance().get(&1u32).unwrap(),
            )
        }

        pub fn set_tokens(env: Env, token_a: Address, token_b: Address) {
            env.storage().instance().set(&0u32, &token_a);
            env.storage().instance().set(&1u32, &token_b);
        }

        pub fn fee_bps(_env: Env) -> i128 {
            TEST_FEE_BPS
        }
    }

    const TEST_FEE_BPS: i128 = 30;

    /// Factory registry stub: records the pools the test registers and
    /// answers the four lookups the migration contract makes.
    #[contracttype]
    enum FKey {
        Pool(Address, Address),
        PoolTokens(Address),
        ClPool(Address, Address, i128),
        IsCl(Address),
    }

    #[contract]
    struct MockFactory;

    fn sorted(a: Address, b: Address) -> (Address, Address) {
        if a < b {
            (a, b)
        } else {
            (b, a)
        }
    }

    #[contractimpl]
    impl MockFactory {
        pub fn reg_v2(env: Env, pool: Address, token_a: Address, token_b: Address) {
            let (a, b) = sorted(token_a.clone(), token_b.clone());
            env.storage().persistent().set(&FKey::Pool(a, b), &pool);
            env.storage()
                .persistent()
                .set(&FKey::PoolTokens(pool), &(token_a, token_b));
        }

        pub fn reg_cl(env: Env, pool: Address, token_a: Address, token_b: Address, fee: i128) {
            let (a, b) = sorted(token_a, token_b);
            env.storage()
                .persistent()
                .set(&FKey::ClPool(a, b, fee), &pool);
            env.storage().persistent().set(&FKey::IsCl(pool), &true);
        }

        pub fn get_pool(env: Env, token_a: Address, token_b: Address) -> Option<Address> {
            let (a, b) = sorted(token_a, token_b);
            env.storage().persistent().get(&FKey::Pool(a, b))
        }

        pub fn get_cl_pool(
            env: Env,
            token_a: Address,
            token_b: Address,
            fee_bps: i128,
        ) -> Option<Address> {
            let (a, b) = sorted(token_a, token_b);
            env.storage().persistent().get(&FKey::ClPool(a, b, fee_bps))
        }

        pub fn get_pool_tokens(env: Env, pool: Address) -> Option<(Address, Address)> {
            env.storage().persistent().get(&FKey::PoolTokens(pool))
        }

        pub fn is_cl_pool(env: Env, pool: Address) -> bool {
            env.storage()
                .persistent()
                .get(&FKey::IsCl(pool))
                .unwrap_or(false)
        }
    }

    /// Minimal V2 pool stub: only `get_info` is needed so `initialize`'s
    /// token-pair validation passes; `preview_range` never calls it.
    #[contract]
    struct MockV2Pool;

    #[contractimpl]
    impl MockV2Pool {
        pub fn get_info(env: Env) -> V2PoolInfo {
            let token_a: Address = env.storage().instance().get(&0u32).unwrap();
            let token_b: Address = env.storage().instance().get(&1u32).unwrap();
            V2PoolInfo {
                token_a,
                token_b,
                reserve_a: 0,
                reserve_b: 0,
                total_shares: 0,
                fee_bps: 0,
                flash_loan_fee_bps: 0,
                admin: env.current_contract_address(),
                fee_recipient: env.current_contract_address(),
                protocol_fee_bps: 0,
                lp_rebate_bps: 0,
            }
        }

        pub fn set_v2_tokens(env: Env, token_a: Address, token_b: Address) {
            env.storage().instance().set(&0u32, &token_a);
            env.storage().instance().set(&1u32, &token_b);
        }
    }

    fn setup() -> (Env, Address) {
        let env = Env::default();
        env.mock_all_auths();

        let admin = Address::generate(&env);
        let token_a = Address::generate(&env);
        let token_b = Address::generate(&env);

        let v2_pool = env.register_contract(None, MockV2Pool);
        MockV2PoolClient::new(&env, &v2_pool).set_v2_tokens(&token_a, &token_b);

        let v3_pool = env.register_contract(None, RangePool);
        RangePoolClient::new(&env, &v3_pool).set_tokens(&token_a, &token_b);

        let factory = env.register_contract(None, MockFactory);
        let registry = MockFactoryClient::new(&env, &factory);
        registry.reg_v2(&v2_pool, &token_a, &token_b);
        registry.reg_cl(&v3_pool, &token_a, &token_b, &TEST_FEE_BPS);

        let contract_addr = env.register_contract(None, MigrationContract);
        let client = MigrationContractClient::new(&env, &contract_addr);
        client.initialize(&admin, &factory);
        client.set_default_pair(&v2_pool, &v3_pool);

        (env, contract_addr)
    }

    #[test]
    fn both_explicit_ticks_are_kept_as_is() {
        let (env, contract_addr) = setup();

        let client = MigrationContractClient::new(&env, &contract_addr);

        let result = client.preview_range_default(&100, &200, &0);

        assert_eq!(result, (100, 200));
    }

    #[test]
    fn both_sentinels_auto_compute_symmetric_range() {
        let (env, contract_addr) = setup();

        let client = MigrationContractClient::new(&env, &contract_addr);

        let result = client.preview_range_default(&i32::MIN, &i32::MAX, &50);

        assert_eq!(result, (950, 1050));
    }

    /// Regression test for issue #478: an explicit `tick_lower` combined with
    /// `tick_upper = i32::MAX` must auto-compute only the upper bound and
    /// keep the caller's explicit lower bound, rather than silently
    /// discarding it in favor of a fully symmetric range.
    #[test]
    fn explicit_lower_with_auto_upper_keeps_explicit_lower() {
        let (env, contract_addr) = setup();

        let client = MigrationContractClient::new(&env, &contract_addr);

        let result = client.preview_range_default(&500, &i32::MAX, &50);

        assert_eq!(result, (500, 1050));
    }

    /// Mirror case: auto lower bound with an explicit upper bound.
    #[test]
    fn auto_lower_with_explicit_upper_keeps_explicit_upper() {
        let (env, contract_addr) = setup();

        let client = MigrationContractClient::new(&env, &contract_addr);

        let result = client.preview_range_default(&i32::MIN, &1500, &50);

        assert_eq!(result, (950, 1500));
    }

    #[test]
    fn partial_auto_range_rejects_non_positive_width() {
        let (env, contract_addr) = setup();

        let client = MigrationContractClient::new(&env, &contract_addr);

        let result = client.try_preview_range_default(&500, &i32::MAX, &0);

        assert_eq!(result, Err(Ok(MigrationError::InvalidRange)));
    }

    #[test]
    fn partial_auto_range_rejects_inverted_result() {
        let (env, contract_addr) = setup();

        let client = MigrationContractClient::new(&env, &contract_addr);

        // Explicit lower (2000) ends up above the auto-computed upper
        // (current_tick + 50 = 1050); this must be rejected, not silently
        // deposited into an inverted/degenerate range.
        let result = client.try_preview_range_default(&2_000, &i32::MAX, &50);

        assert_eq!(result, Err(Ok(MigrationError::InvalidRange)));
    }

    #[test]
    fn explicit_ticks_reject_inverted_range() {
        let (env, contract_addr) = setup();

        let client = MigrationContractClient::new(&env, &contract_addr);

        let result = client.try_preview_range_default(&200, &100, &0);

        assert_eq!(result, Err(Ok(MigrationError::InvalidRange)));
    }

    // ══════════════════════════════════════════════════════════════════════════
    // Issue #686: full migration unit-test suite against real contracts.
    //
    // The tests below register the REAL `amm` V2 pool plus real SEP-41 token
    // contracts (`StellarAssetClient`) in the test Env, so the funds-moving
    // cross-contract calls — `remove_liquidity`, `transfer`, `approve`,
    // `transfer_from`, and LpToken `mint`/`burn` — all run against deployed
    // contracts rather than hand-rolled stubs.
    //
    // A small CL-ABI harness keeps the unit tests focused; the integration suite
    // registers the real ConcentratedLiquidity contract for end-to-end coverage.
    mod test_v3_pool {
        use super::*;

        #[contract]
        pub(crate) struct TestV3Pool;

        #[contracttype]
        #[derive(Clone, Debug, PartialEq)]
        pub(crate) struct V3Position {
            pub(crate) provider: Address,
            pub(crate) tick_lower: i32,
            pub(crate) tick_upper: i32,
            pub(crate) deposited_a: i128,
            pub(crate) deposited_b: i128,
        }

        #[contracttype]
        enum V3DataKey {
            TokensA,
            TokensB,
            CurrentTick,
            NextPositionId,
            Position(u64),
            PositionId(Address, i32, i32),
        }

        #[contractimpl]
        impl TestV3Pool {
            pub fn setup(env: Env, token_a: Address, token_b: Address, initial_tick: i32) {
                env.storage().instance().set(&V3DataKey::TokensA, &token_a);
                env.storage().instance().set(&V3DataKey::TokensB, &token_b);
                env.storage()
                    .instance()
                    .set(&V3DataKey::CurrentTick, &initial_tick);
                env.storage()
                    .instance()
                    .set(&V3DataKey::NextPositionId, &0_u64);
            }

            pub fn set_current_tick(env: Env, tick: i32) {
                env.storage().instance().set(&V3DataKey::CurrentTick, &tick);
            }

            pub fn get_pool_state(env: Env) -> PoolState {
                let current_tick = env
                    .storage()
                    .instance()
                    .get(&V3DataKey::CurrentTick)
                    .unwrap_or(0);
                PoolState {
                    sqrt_price: 0,
                    current_tick,
                    active_liquidity: 0,
                    tick_spacing: 1,
                }
            }

            pub fn get_tokens(env: Env) -> (Address, Address) {
                let token_a: Address = env.storage().instance().get(&V3DataKey::TokensA).unwrap();
                let token_b: Address = env.storage().instance().get(&V3DataKey::TokensB).unwrap();
                (token_a, token_b)
            }

            pub fn fee_bps(_env: Env) -> i128 {
                TEST_FEE_BPS
            }

            /// Deterministic quote model: below range only token A, above
            /// range only token B, in range `(L, 2L)`.
            pub fn quote_position(
                env: Env,
                lower_tick: i32,
                upper_tick: i32,
                liquidity: i128,
            ) -> Result<(i128, i128), soroban_sdk::Error> {
                if lower_tick >= upper_tick || liquidity <= 0 {
                    return Err(soroban_sdk::Error::from_contract_error(3));
                }
                let current: i32 = env
                    .storage()
                    .instance()
                    .get(&V3DataKey::CurrentTick)
                    .unwrap_or(0);
                Ok(if current < lower_tick {
                    (liquidity, 0)
                } else if current >= upper_tick {
                    (0, liquidity)
                } else {
                    (liquidity, 2 * liquidity)
                })
            }

            pub fn position(env: Env, position_id: u64) -> V3Position {
                env.storage()
                    .instance()
                    .get(&V3DataKey::Position(position_id))
                    .unwrap()
            }

            /// Number of positions minted so far (0 before the first deposit).
            pub fn position_count(env: Env) -> i128 {
                let next: u64 = env
                    .storage()
                    .instance()
                    .get(&V3DataKey::NextPositionId)
                    .unwrap_or(0);
                i128::from(next)
            }

            /// Mirrors the real CL `mint_position` surface and direct-provider
            /// token flow while keeping the unit-test math deterministic.
            #[allow(clippy::too_many_arguments)]
            pub fn mint_position(
                env: Env,
                provider: Address,
                tick_lower: i32,
                tick_upper: i32,
                amount_a: i128,
                amount_b: i128,
                min_a: i128,
                min_b: i128,
                _deadline: u64,
            ) -> Result<(i128, i128), soroban_sdk::Error> {
                if tick_lower >= tick_upper {
                    return Err(soroban_sdk::Error::from_contract_error(1));
                }
                if amount_a < min_a || amount_b < min_b {
                    return Err(soroban_sdk::Error::from_contract_error(2));
                }
                provider.require_auth();

                let token_a: Address = env.storage().instance().get(&V3DataKey::TokensA).unwrap();
                let token_b: Address = env.storage().instance().get(&V3DataKey::TokensB).unwrap();
                let self_addr = env.current_contract_address();

                if amount_a > 0 {
                    TokenClient::new(&env, &token_a).transfer(&provider, &self_addr, &amount_a);
                }
                if amount_b > 0 {
                    TokenClient::new(&env, &token_b).transfer(&provider, &self_addr, &amount_b);
                }

                let next: u64 = env
                    .storage()
                    .instance()
                    .get(&V3DataKey::NextPositionId)
                    .unwrap_or(0);
                env.storage().instance().set(
                    &V3DataKey::Position(next),
                    &V3Position {
                        provider: provider.clone(),
                        tick_lower,
                        tick_upper,
                        deposited_a: amount_a,
                        deposited_b: amount_b,
                    },
                );
                env.storage()
                    .instance()
                    .set(&V3DataKey::NextPositionId, &(next + 1));
                env.storage().instance().set(
                    &V3DataKey::PositionId(provider, tick_lower, tick_upper),
                    &next,
                );

                Ok((amount_a, amount_b))
            }

            pub fn position_token_id(
                env: Env,
                provider: Address,
                tick_lower: i32,
                tick_upper: i32,
            ) -> Option<u64> {
                env.storage()
                    .instance()
                    .get(&V3DataKey::PositionId(provider, tick_lower, tick_upper))
            }
        }
    }

    pub(crate) use test_v3_pool::{TestV3Pool, TestV3PoolClient};

    // ── Shared fixture ─────────────────────────────────────────────────────────

    const DEADLINE: u64 = u64::MAX;

    struct Fixture<'a> {
        admin: Address,
        lp: Address,
        lp_shares: i128,
        ta: TokenClient<'a>,
        tb: TokenClient<'a>,
        ta_sac: StellarAssetClient<'a>,
        tb_sac: StellarAssetClient<'a>,
        v2_pool: Address,
        v2_lp: LpTokenClient<'a>,
        v3_pool: Address,
        v3: TestV3PoolClient<'a>,
        migration: MigrationContractClient<'a>,
        migration_addr: Address,
        factory: Address,
        registry: MockFactoryClient<'a>,
    }

    fn create_sac<'a>(env: &'a Env, admin: &Address) -> (TokenClient<'a>, StellarAssetClient<'a>) {
        let c = env.register_stellar_asset_contract_v2(admin.clone());
        (
            TokenClient::new(env, &c.address()),
            StellarAssetClient::new(env, &c.address()),
        )
    }

    /// Bootstrap a full fixture around the REAL V2 AMM:
    ///   - `admin` seeds the pool with 6_000_000/6_000_000 (the first deposit
    ///     locks MINIMUM_LIQUIDITY = 1_000; admin receives 5_999_000 LP);
    ///   - `lp` mints exactly `lp_deposit_a`/`lp_deposit_b` and deposits
    ///     them, ending with reserves 7_000_000/7_000_000 (+lp_deposit_b)
    ///     and total shares 7_000_000 (+lp_deposit_b);
    ///   - the registered `TestV3Pool` harness is wired to the pair in the
    ///     `reversed` order when requested;
    ///   - the migration contract is initialized against both pools.
    fn build_fixture(
        env: &Env,
        lp_deposit_a: i128,
        lp_deposit_b: i128,
        reversed: bool,
        initial_tick: i32,
    ) -> Fixture<'_> {
        env.budget().reset_unlimited();
        env.mock_all_auths();

        let admin = Address::generate(env);
        let lp = Address::generate(env);
        let fee_recipient = Address::generate(env);

        let (ta, ta_sac) = create_sac(env, &admin);
        let (tb, tb_sac) = create_sac(env, &admin);

        // ── Real V2 AMM pool + real LpToken ──
        let v2_addr = env.register_contract(None, AmmPool);
        let v2_lp_addr = env.register_contract(None, LpToken);
        LpTokenClient::new(env, &v2_lp_addr).initialize(
            &v2_addr,
            &String::from_str(env, "V2 LP"),
            &String::from_str(env, "V2LP"),
            &7u32,
        );
        let v2 = AmmPoolClient::new(env, &v2_addr);
        v2.initialize(
            &admin,
            &ta.address,
            &tb.address,
            &v2_lp_addr,
            &30_i128,
            &fee_recipient,
            &0_i128,
        );

        ta_sac.mint(&admin, &10_000_000_i128);
        tb_sac.mint(&admin, &10_000_000_i128);
        v2.add_liquidity(&admin, &6_000_000_i128, &6_000_000_i128, &0_i128, &DEADLINE);
        assert_eq!(
            LpTokenClient::new(env, &v2_lp_addr).balance(&admin),
            5_999_000_i128,
            "6M initial shares minus the 1_000 locked MINIMUM_LIQUIDITY"
        );

        ta_sac.mint(&lp, &lp_deposit_a);
        tb_sac.mint(&lp, &lp_deposit_b);
        let lp_shares = v2.add_liquidity(&lp, &lp_deposit_a, &lp_deposit_b, &0_i128, &DEADLINE);

        // ── V3 harness ──
        let v3_addr = env.register_contract(None, TestV3Pool);
        let v3 = TestV3PoolClient::new(env, &v3_addr);
        if reversed {
            v3.setup(&tb.address, &ta.address, &initial_tick);
        } else {
            v3.setup(&ta.address, &tb.address, &initial_tick);
        }

        // ── Factory registry + migration contract ──
        let factory_addr = env.register_contract(None, MockFactory);
        let registry = MockFactoryClient::new(env, &factory_addr);
        registry.reg_v2(&v2_addr, &ta.address, &tb.address);
        registry.reg_cl(&v3_addr, &ta.address, &tb.address, &TEST_FEE_BPS);

        let migration_addr = env.register_contract(None, MigrationContract);
        let migration = MigrationContractClient::new(env, &migration_addr);
        migration.initialize(&admin, &factory_addr);
        migration.set_default_pair(&v2_addr, &v3_addr);

        Fixture {
            admin,
            lp,
            lp_shares,
            ta,
            tb,
            ta_sac,
            tb_sac,
            v2_pool: v2_addr,
            v2_lp: LpTokenClient::new(env, &v2_lp_addr),
            v3_pool: v3_addr,
            v3,
            migration,
            migration_addr,
            factory: factory_addr,
            registry,
        }
    }

    /// Default symmetric fixture:
    /// reserves 7_000_000/7_000_000, total shares 7_000_000, `lp` holds
    /// 1_000_000 shares (a full exit withdraws exactly 1_000_000/1_000_000).
    fn fixture(env: &Env) -> Fixture<'_> {
        build_fixture(env, 1_000_000, 1_000_000, false, 0)
    }

    // ── initialize ─────────────────────────────────────────────────────────────

    #[test]
    fn test_initialize_twice_returns_already_initialized() {
        let env = Env::default();
        let f = fixture(&env);

        let second = f.migration.try_initialize(&f.admin, &f.factory);

        assert!(
            matches!(second, Err(Ok(MigrationError::AlreadyInitialized))),
            "re-initializing must be rejected"
        );

        // The pool addresses survive re-initialization attempts: preview_range
        // reads the stored V3 address and round-trips real pool state.
        assert_eq!(
            f.migration.preview_range_default(&100, &200, &0),
            (100, 200)
        );
    }

    #[test]
    fn test_set_default_pair_rejects_mismatched_v3_token_pair() {
        let env = Env::default();
        let f = fixture(&env);

        let (tc, _tc_sac) = create_sac(&env, &f.admin);
        let bad_v3 = env.register_contract(None, TestV3Pool);
        TestV3PoolClient::new(&env, &bad_v3).setup(&f.ta.address, &tc.address, &0);
        f.registry
            .reg_cl(&bad_v3, &f.ta.address, &tc.address, &TEST_FEE_BPS);

        let result = f.migration.try_set_default_pair(&f.v2_pool, &bad_v3);

        assert!(
            matches!(
                result,
                Err(Ok(
                    MigrationError::UnknownPool | MigrationError::TokenMismatch
                ))
            ),
            "a V3 pool trading a different pair must be rejected"
        );
    }

    #[test]
    fn test_migrate_before_initialize_returns_not_initialized() {
        let env = Env::default();
        let provider = Address::generate(&env);
        let contract_addr = env.register_contract(None, MigrationContract);
        let client = MigrationContractClient::new(&env, &contract_addr);

        let result = client.try_migrate_default(
            &provider, &1_i128, &0_i128, &0_i128, &100_i32, &200_i32, &0_i32, &0_i128, &0_i128,
            &DEADLINE,
        );
        assert!(
            matches!(result, Err(Ok(MigrationError::NotInitialized))),
            "migrate before initialize must fail"
        );

        let preview = client.try_preview_range_default(&100, &200, &0);
        assert!(
            matches!(preview, Err(Ok(MigrationError::NotInitialized))),
            "preview_range before initialize must fail"
        );
    }

    // ── preview_range against the registered V3 harness ─────────────────────────

    #[test]
    fn test_preview_range_explicit_ticks_used_verbatim() {
        let env = Env::default();
        let f = fixture(&env);
        let client = MigrationContractClient::new(&env, &f.migration_addr);

        // Explicit bounds are kept exactly as-is regardless of the pool tick.
        assert_eq!(
            f.migration.preview_range_default(&100, &200, &9_999),
            (100, 200)
        );

        // Inverted explicit bounds are rejected.
        let result = client.try_preview_range_default(&200, &100, &0);
        assert!(matches!(result, Err(Ok(MigrationError::InvalidRange))));
    }

    #[test]
    fn test_preview_range_sentinels_auto_bracket_current_tick() {
        let env = Env::default();
        let f = fixture(&env);

        f.v3.set_current_tick(&1_000_i32);

        assert_eq!(
            f.migration
                .preview_range_default(&i32::MIN, &i32::MAX, &500),
            (500, 1_500)
        );

        // Single-sided sentinels only replace that bound, keeping the other.
        assert_eq!(
            f.migration.preview_range_default(&i32::MIN, &1_500, &500),
            (500, 1_500)
        );
        assert_eq!(
            f.migration.preview_range_default(&500, &i32::MAX, &500),
            (500, 1_500)
        );
    }

    #[test]
    fn test_preview_range_pool_without_liquidity_still_returns_valid_range() {
        let env = Env::default();
        let f = fixture(&env);

        // No position deposited yet; the harness reports its stored current
        // tick (0) and the migration still yields a well-formed, non-empty
        // bracket rather than panicking or returning an inverted range.
        assert_eq!(
            f.migration
                .preview_range_default(&i32::MIN, &i32::MAX, &100),
            (-100, 100)
        );
    }

    #[test]
    fn test_preview_range_wider_width_auto_range_strictly_contains_narrower() {
        let env = Env::default();
        let f = fixture(&env);

        f.v3.set_current_tick(&0_i32);

        let narrow = f
            .migration
            .preview_range_default(&i32::MIN, &i32::MAX, &100);
        let wide = f
            .migration
            .preview_range_default(&i32::MIN, &i32::MAX, &1_000);

        assert_eq!(narrow, (-100, 100));
        assert_eq!(wide, (-1_000, 1_000));
        assert!(
            narrow.0 > wide.0 && narrow.1 < wide.1,
            "the wider range must strictly contain the narrower one"
        );
    }

    // ── migrate happy paths ─────────────────────────────────────────────────────

    #[test]
    fn test_full_migration_burns_all_lp_shares_and_mints_position() {
        let env = Env::default();
        let f = fixture(&env);

        assert_eq!(f.v2_lp.balance(&f.lp), 1_000_000_i128);

        let result = f.migration.migrate_default(
            &f.lp,
            &f.lp_shares,
            &0_i128,
            &0_i128,
            &i32::MIN,
            &i32::MAX,
            &500_i32,
            &0_i128,
            &0_i128,
            &DEADLINE,
        );

        // All V2 LP shares were burned by the real AMM contract.
        assert_eq!(f.v2_lp.balance(&f.lp), 0_i128);
        assert_eq!(f.v3.position_count(), 1_i128);

        // The registered V3 pool minted a real, queryable position at the
        // range auto-computed around the harness's current tick (default 0).
        let pos = f.v3.position(&result.position_token_id.unwrap());
        assert_eq!(pos.provider, f.lp);
        assert_eq!(pos.tick_lower, -500);
        assert_eq!(pos.tick_upper, 500);
        assert_eq!(pos.deposited_a, 1_000_000_i128);
        assert_eq!(pos.deposited_b, 1_000_000_i128);
    }

    #[test]
    fn test_full_migration_deposits_exact_withdrawal_amounts_with_no_dust() {
        let env = Env::default();
        let f = fixture(&env);

        // shares * reserve_x / total_shares = 1M * 7M / 7M = exactly 1M each.
        let result = f.migration.migrate_default(
            &f.lp,
            &f.lp_shares,
            &0_i128,
            &0_i128,
            &i32::MIN,
            &i32::MAX,
            &500_i32,
            &0_i128,
            &0_i128,
            &DEADLINE,
        );

        assert_eq!(result.deposited_a, 1_000_000_i128);
        assert_eq!(result.deposited_b, 1_000_000_i128);
        assert_eq!(result.leftover_a, 0_i128);
        assert_eq!(result.leftover_b, 0_i128);

        // Both tokens physically landed in the V3 pool.
        assert_eq!(f.ta.balance(&f.v3_pool), 1_000_000_i128);
        assert_eq!(f.tb.balance(&f.v3_pool), 1_000_000_i128);
    }

    #[test]
    fn test_migration_conserves_every_token_and_does_not_touch_admin_position() {
        let env = Env::default();
        let f = fixture(&env);

        let result = f.migration.migrate_default(
            &f.lp,
            &f.lp_shares,
            &0_i128,
            &0_i128,
            &i32::MIN,
            &i32::MAX,
            &500_i32,
            &0_i128,
            &0_i128,
            &DEADLINE,
        );

        // The migrating LP ends up with nothing in their pocket...
        assert_eq!(f.ta.balance(&f.lp), 0_i128);
        assert_eq!(f.tb.balance(&f.lp), 0_i128);

        // ...the shared migration contract retains nothing...
        assert_eq!(f.ta.balance(&f.migration_addr), 0_i128);
        assert_eq!(f.tb.balance(&f.migration_addr), 0_i128);

        // ...and 100% of the withdrawn tokens sit in the V3 pool.
        assert_eq!(f.ta.balance(&f.v3_pool), result.deposited_a);
        assert_eq!(f.tb.balance(&f.v3_pool), result.deposited_b);

        // Per-token conservation: nothing was created or destroyed.
        let lp_original_a = 1_000_000_i128;
        let lp_original_b = 1_000_000_i128;
        assert_eq!(
            f.ta.balance(&f.lp) + f.ta.balance(&f.migration_addr) + f.ta.balance(&f.v3_pool),
            lp_original_a
        );
        assert_eq!(
            f.tb.balance(&f.lp) + f.tb.balance(&f.migration_addr) + f.tb.balance(&f.v3_pool),
            lp_original_b
        );

        // Admin's own V2 position was not swept up by the LP's migration.
        assert_eq!(f.v2_lp.balance(&f.admin), 5_999_000_i128);
    }

    #[test]
    fn test_partial_migration_burns_only_requested_shares() {
        let env = Env::default();
        let f = fixture(&env);

        let half = f.lp_shares / 2;
        let result = f.migration.migrate_default(
            &f.lp,
            &half,
            &0_i128,
            &0_i128,
            &i32::MIN,
            &i32::MAX,
            &500_i32,
            &0_i128,
            &0_i128,
            &DEADLINE,
        );

        // Exactly half the shares burned, the rest stays withdrawable.
        assert_eq!(f.v2_lp.balance(&f.lp), 500_000_i128);
        assert_eq!(result.deposited_a, 500_000_i128);
        assert_eq!(result.deposited_b, 500_000_i128);
        assert_eq!(f.v3.position_count(), 1_i128);

        // The LP redeems the remaining 500k shares directly on the real AMM.
        let v2 = AmmPoolClient::new(&env, &f.v2_pool);
        let (out_a, out_b) = v2.remove_liquidity(&f.lp, &500_000_i128, &0_i128, &0_i128, &DEADLINE);
        assert_eq!((out_a, out_b), (500_000_i128, 500_000_i128));

        assert_eq!(f.v2_lp.balance(&f.lp), 0_i128);
        assert_eq!(f.ta.balance(&f.lp), 500_000_i128);
        assert_eq!(f.tb.balance(&f.lp), 500_000_i128);
    }

    #[test]
    fn test_migrate_preview_ticks_match_executed_position() {
        let env = Env::default();
        let f = fixture(&env);

        f.v3.set_current_tick(&1_000_i32);

        let preview = f
            .migration
            .preview_range_default(&i32::MIN, &i32::MAX, &500);
        assert_eq!(preview, (500, 1_500));

        let result = f.migration.migrate_default(
            &f.lp,
            &f.lp_shares,
            &0_i128,
            &0_i128,
            &i32::MIN,
            &i32::MAX,
            &500_i32,
            &0_i128,
            &0_i128,
            &DEADLINE,
        );

        assert_eq!((result.tick_lower, result.tick_upper), (500, 1_500));

        let pos = f.v3.position(&result.position_token_id.unwrap());
        assert_eq!((pos.tick_lower, pos.tick_upper), (500, 1_500));
    }

    #[test]
    fn test_migration_contract_never_holds_or_approves_tokens() {
        let env = Env::default();
        let f = fixture(&env);

        let _ = f.migration.migrate_default(
            &f.lp,
            &f.lp_shares,
            &0_i128,
            &0_i128,
            &i32::MIN,
            &i32::MAX,
            &500_i32,
            &0_i128,
            &0_i128,
            &DEADLINE,
        );

        // The direct LP-to-CL flow never creates migration-contract approvals.
        assert_eq!(f.ta.allowance(&f.migration_addr, &f.v3_pool), 0_i128);
        assert_eq!(f.tb.allowance(&f.migration_addr, &f.v3_pool), 0_i128);

        // The shared migration contract retained nothing from this call.
        assert_eq!(f.ta.balance(&f.migration_addr), 0_i128);
        assert_eq!(f.tb.balance(&f.migration_addr), 0_i128);

        // Behavioral proof: funds minted later to the contract are not spendable
        // by the V3 pool through any lingering allowance.
        f.ta_sac.mint(&f.migration_addr, &100_i128);
        let steal =
            f.ta.try_transfer_from(&f.v3_pool, &f.migration_addr, &f.lp, &100_i128);
        assert!(
            steal.is_err(),
            "v3_pool must not be able to move post-migration balances"
        );
        assert_eq!(f.ta.balance(&f.migration_addr), 100_i128);

        // Same guarantee for token B.
        f.tb_sac.mint(&f.migration_addr, &100_i128);
        let steal_b =
            f.tb.try_transfer_from(&f.v3_pool, &f.migration_addr, &f.lp, &100_i128);
        assert!(
            steal_b.is_err(),
            "v3_pool must not be able to move post-migration balances"
        );
        assert_eq!(f.tb.balance(&f.migration_addr), 100_i128);
    }

    // ── migrate failure paths and atomicity ─────────────────────────────────────

    #[test]
    fn test_migrate_zero_shares_is_rejected() {
        let env = Env::default();
        let f = fixture(&env);

        let result = f.migration.try_migrate_default(
            &f.lp,
            &0_i128,
            &0_i128,
            &0_i128,
            &i32::MIN,
            &i32::MAX,
            &500_i32,
            &0_i128,
            &0_i128,
            &DEADLINE,
        );
        assert!(matches!(result, Err(Ok(MigrationError::ZeroShares))));

        let negative = f.migration.try_migrate_default(
            &f.lp,
            &-1_i128,
            &0_i128,
            &0_i128,
            &i32::MIN,
            &i32::MAX,
            &500_i32,
            &0_i128,
            &0_i128,
            &DEADLINE,
        );
        assert!(matches!(negative, Err(Ok(MigrationError::ZeroShares))));
    }

    #[test]
    fn test_migrate_more_shares_than_owned_aborts_atomically() {
        let env = Env::default();
        let f = fixture(&env);

        let before_shares = f.v2_lp.balance(&f.lp);
        let before_ta = f.ta.balance(&f.lp);
        let before_tb = f.tb.balance(&f.lp);

        let result = f.migration.try_migrate_default(
            &f.lp,
            &(f.lp_shares + 1),
            &0_i128,
            &0_i128,
            &i32::MIN,
            &i32::MAX,
            &500_i32,
            &0_i128,
            &0_i128,
            &DEADLINE,
        );
        assert!(result.is_err(), "over-owning migration must panic/revert");

        // Nothing moved: V2 shares, the LP's tokens and the V3 pool are untouched.
        assert_eq!(f.v2_lp.balance(&f.lp), before_shares);
        assert_eq!(f.ta.balance(&f.lp), before_ta);
        assert_eq!(f.tb.balance(&f.lp), before_tb);
        assert_eq!(f.ta.balance(&f.migration_addr), 0_i128);
        assert_eq!(f.tb.balance(&f.migration_addr), 0_i128);
        assert_eq!(f.v3.position_count(), 0_i128);
    }

    #[test]
    fn test_migrate_past_deadline_aborts_atomically() {
        let env = Env::default();
        let f = fixture(&env);

        env.ledger().set_timestamp(1_000);

        let before_shares = f.v2_lp.balance(&f.lp);
        let before_ta = f.ta.balance(&f.lp);
        let before_tb = f.tb.balance(&f.lp);

        let result = f.migration.try_migrate_default(
            &f.lp,
            &f.lp_shares,
            &0_i128,
            &0_i128,
            &i32::MIN,
            &i32::MAX,
            &500_i32,
            &0_i128,
            &0_i128,
            &999_u64,
        );
        assert!(result.is_err(), "expired deadline must abort the migration");

        assert_eq!(f.v2_lp.balance(&f.lp), before_shares);
        assert_eq!(f.ta.balance(&f.lp), before_ta);
        assert_eq!(f.tb.balance(&f.lp), before_tb);
        assert_eq!(f.v3.position_count(), 0_i128);
    }

    #[test]
    fn test_migrate_v2_slippage_aborts_atomically() {
        let env = Env::default();
        let f = fixture(&env);

        let before_shares = f.v2_lp.balance(&f.lp);

        // min_amount_a impossibly high: the real AMM rejects the withdrawal.
        let result = f.migration.try_migrate_default(
            &f.lp,
            &f.lp_shares,
            &i128::MAX,
            &0_i128,
            &i32::MIN,
            &i32::MAX,
            &500_i32,
            &0_i128,
            &0_i128,
            &DEADLINE,
        );
        assert!(result.is_err(), "impossible V2 slippage must revert");

        assert_eq!(f.v2_lp.balance(&f.lp), before_shares);
        assert_eq!(f.v3.position_count(), 0_i128);
        assert_eq!(f.ta.balance(&f.migration_addr), 0_i128);
        assert_eq!(f.tb.balance(&f.migration_addr), 0_i128);
    }

    /// The atomicity guarantee under the issue's exact scenario: the V2 side
    /// fully succeeds (shares burned and tokens withdrawn by the real AMM), but
    /// the V3 deposit later fails on `min_deposit_a` — the whole migration must
    /// roll back to exactly the pre-call state, including re-minting the burned
    /// V2 LP shares and restoring every token balance.
    #[test]
    fn test_migrate_v3_slippage_aborts_entire_migration_atomically() {
        let env = Env::default();
        let f = fixture(&env);

        let before_shares = f.v2_lp.balance(&f.lp);
        let before_ta = f.ta.balance(&f.lp);
        let before_tb = f.tb.balance(&f.lp);
        let before_ta_contract = f.ta.balance(&f.migration_addr);
        let before_tb_contract = f.tb.balance(&f.migration_addr);

        let v2 = AmmPoolClient::new(&env, &f.v2_pool);
        let before_info = v2.get_info();

        let result = f.migration.try_migrate_default(
            &f.lp,
            &f.lp_shares,
            &0_i128,
            &0_i128,
            &i32::MIN,
            &i32::MAX,
            &500_i32,
            &i128::MAX, // impossible: no position can satisfy this floor
            &0_i128,
            &DEADLINE,
        );
        assert!(
            result.is_err(),
            "a V3 deposit below min_deposit_a must abort the migration"
        );

        // Zero state changes: the burned shares were re-minted by the rollback.
        assert_eq!(f.v2_lp.balance(&f.lp), before_shares);
        assert_eq!(f.ta.balance(&f.lp), before_ta);
        assert_eq!(f.tb.balance(&f.lp), before_tb);
        assert_eq!(f.ta.balance(&f.migration_addr), before_ta_contract);
        assert_eq!(f.tb.balance(&f.migration_addr), before_tb_contract);

        let after_info = v2.get_info();
        assert_eq!(after_info.reserve_a, before_info.reserve_a);
        assert_eq!(after_info.reserve_b, before_info.reserve_b);
        assert_eq!(after_info.total_shares, before_info.total_shares);

        // The V3 pool never saw the funds: no position, no token balance.
        assert_eq!(f.v3.position_count(), 0_i128);
        assert_eq!(f.ta.balance(&f.v3_pool), 0_i128);
        assert_eq!(f.tb.balance(&f.v3_pool), 0_i128);

        // And a follow-up migration still succeeds from the untouched state.
        let retry = f.migration.migrate_default(
            &f.lp,
            &f.lp_shares,
            &0_i128,
            &0_i128,
            &i32::MIN,
            &i32::MAX,
            &500_i32,
            &0_i128,
            &0_i128,
            &DEADLINE,
        );
        assert_eq!(f.v2_lp.balance(&f.lp), 0_i128);
        assert_eq!(f.v3.position_count(), 1_i128);
        assert_eq!(retry.deposited_a + retry.deposited_b, 2_000_000_i128);
    }

    #[test]
    fn test_migrate_with_wrong_address_auth_is_rejected() {
        let env = Env::default();
        env.budget().reset_unlimited();

        let admin = Address::generate(&env);
        let provider = Address::generate(&env);
        let attacker = Address::generate(&env);
        let ta = Address::generate(&env);
        let tb = Address::generate(&env);

        let v2_mock = env.register_contract(None, MockV2Pool);
        MockV2PoolClient::new(&env, &v2_mock).set_v2_tokens(&ta, &tb);
        let v3_mock = env.register_contract(None, RangePool);
        RangePoolClient::new(&env, &v3_mock).set_tokens(&ta, &tb);

        let factory = env.register_contract(None, MockFactory);
        let registry = MockFactoryClient::new(&env, &factory);
        registry.reg_v2(&v2_mock, &ta, &tb);
        registry.reg_cl(&v3_mock, &ta, &tb, &TEST_FEE_BPS);

        let contract_addr = env.register_contract(None, MigrationContract);
        let client = MigrationContractClient::new(&env, &contract_addr);

        // Real auth checking: only admin, and only for initialize and the
        // default-pair setup.
        env.mock_auths(&[MockAuth {
            address: &admin,
            invoke: &MockAuthInvoke {
                contract: &contract_addr,
                fn_name: "initialize",
                args: (admin.clone(), factory.clone()).into_val(&env),
                sub_invokes: &[],
            },
        }]);
        assert_eq!(
            client.try_initialize(&admin, &factory),
            Ok(Ok(())),
            "initialize with an authorized admin must succeed"
        );
        env.mock_auths(&[MockAuth {
            address: &admin,
            invoke: &MockAuthInvoke {
                contract: &contract_addr,
                fn_name: "set_default_pair",
                args: (v2_mock.clone(), v3_mock.clone()).into_val(&env),
                sub_invokes: &[],
            },
        }]);
        client.set_default_pair(&v2_mock, &v3_mock);

        // Only the *wrong* address is authorized for the migration call: the LP
        // (provider) is not, so `provider.require_auth()` fails immediately.
        env.mock_auths(&[MockAuth {
            address: &attacker,
            invoke: &MockAuthInvoke {
                contract: &contract_addr,
                fn_name: "migrate_default",
                args: (
                    provider.clone(),
                    1_i128,
                    0_i128,
                    0_i128,
                    100_i32,
                    200_i32,
                    0_i32,
                    0_i128,
                    0_i128,
                    DEADLINE,
                )
                    .into_val(&env),
                sub_invokes: &[],
            },
        }]);
        let result = client.try_migrate_default(
            &provider, &1_i128, &0_i128, &0_i128, &100_i32, &200_i32, &0_i32, &0_i128, &0_i128,
            &DEADLINE,
        );
        assert!(
            result.is_err(),
            "migration without the provider's authorization must fail"
        );

        // The contract was not corrupted: it still serves read-only previews.
        assert_eq!(client.preview_range_default(&100, &200, &0), (100, 200));
    }

    /// Reversed V2/V3 token pairs are mapped into the CL pool's token order.
    #[test]
    fn test_migrate_reversed_v3_token_order_maps_amounts_correctly() {
        let env = Env::default();
        // Asymmetric deposit so amount_a != amount_b, making any swap bug
        // observable instead of silently self-cancelling:
        //   admin: 6M/6M → reserves 6M/6M, total 6M shares (5_999_000 + 1_000);
        //   lp:    1M/2M → shares = min(1M*6M/6M, 2M*6M/6M) = 1M;
        //   reserves 7M/8M, total shares 7M.
        // Full exit: out_a = 1M*7M/7M = 1_000_000, out_b = 1M*8M/7M = 1_142_857.
        let f = build_fixture(&env, 1_000_000, 2_000_000, true, 0);

        let result = f.migration.migrate_default(
            &f.lp,
            &f.lp_shares,
            &0_i128,
            &0_i128,
            &i32::MIN,
            &i32::MAX,
            &500_i32,
            &0_i128,
            &0_i128,
            &DEADLINE,
        );

        // The reversed V3 pool (token_a == V2's token_b) must receive the tb
        // side as its amount_a and the ta side as its amount_b.
        assert_eq!(result.deposited_a, 1_142_857_i128);
        assert_eq!(result.deposited_b, 1_000_000_i128);

        let pos = f.v3.position(&result.position_token_id.unwrap());
        assert_eq!(pos.deposited_a, 1_142_857_i128);
        assert_eq!(pos.deposited_b, 1_000_000_i128);

        // The underlying tokens landed on the correct sides: V3's token_a (==
        // V2's token_b) is ahead by 1_142_857; V3's token_b (== V2's token_a)
        // by 1_000_000.
        assert_eq!(f.tb.balance(&f.v3_pool), 1_142_857_i128);
        assert_eq!(f.ta.balance(&f.v3_pool), 1_000_000_i128);
    }

    #[test]
    fn test_migrated_event_emits_versioned_schema() {
        let env = Env::default();
        env.mock_all_auths();
        let f = build_fixture(&env, 1_000_000_i128, 1_000_000_i128, false, 1000);

        let _result = f.migration.migrate_default(
            &f.lp,
            &500_000_i128,
            &0_i128,
            &0_i128,
            &i32::MIN,
            &i32::MAX,
            &50_i32,
            &0_i128,
            &0_i128,
            &DEADLINE,
        );

        // Read all events and find the migrated event
        let events = env.events().all();
        let migrated_events: std::vec::Vec<_> = events
            .iter()
            .filter(|event| {
                if let Some(topic_val) = event.1.get(0) {
                    Symbol::try_from_val(&env, &topic_val) == Ok(Symbol::new(&env, "migrated"))
                } else {
                    false
                }
            })
            .collect();

        assert!(
            !migrated_events.is_empty(),
            "migrated event must be emitted"
        );

        let event = migrated_events.last().unwrap();
        type MigratedEvent = (u32, (i128, i128, i128, Option<u64>, i128, i128));
        let (
            version,
            (_v2_shares, _deposited_a, _deposited_b, _position_token_id, _leftover_a, _leftover_b),
        ): MigratedEvent = MigratedEvent::try_from_val(&env, &event.2)
            .expect("must decode event with version prefix");
        assert_eq!(
            version,
            soroban_amm_sdk::EVENT_SCHEMA_VERSION,
            "event must have correct schema version prefix"
        );
    }

    // ══════════════════════════════════════════════════════════════════════════
    // Issue #1150: one instance for every pair, factory-attested pools, admin
    // powers, and `preview_migration`.

    struct Pair<'a> {
        ta: TokenClient<'a>,
        tb: TokenClient<'a>,
        v2_pool: Address,
        v2_lp: LpTokenClient<'a>,
        v3_pool: Address,
        v3: TestV3PoolClient<'a>,
        shares: i128,
    }

    /// Register a second, independent pair (new tokens, new V2 pool, new CL
    /// harness) in the fixture's env and factory registry, and give `f.lp`
    /// 2_000_000 shares of the new V2 pool.
    fn add_pair<'a>(env: &'a Env, f: &Fixture<'a>) -> Pair<'a> {
        let fee_recipient = Address::generate(env);
        let (ta, ta_sac) = create_sac(env, &f.admin);
        let (tb, tb_sac) = create_sac(env, &f.admin);

        let v2_pool = env.register_contract(None, AmmPool);
        let v2_lp_addr = env.register_contract(None, LpToken);
        LpTokenClient::new(env, &v2_lp_addr).initialize(
            &v2_pool,
            &String::from_str(env, "V2 LP 2"),
            &String::from_str(env, "V2LP2"),
            &7u32,
        );
        let v2 = AmmPoolClient::new(env, &v2_pool);
        v2.initialize(
            &f.admin,
            &ta.address,
            &tb.address,
            &v2_lp_addr,
            &30_i128,
            &fee_recipient,
            &0_i128,
        );
        ta_sac.mint(&f.admin, &6_000_000_i128);
        tb_sac.mint(&f.admin, &6_000_000_i128);
        v2.add_liquidity(
            &f.admin,
            &6_000_000_i128,
            &6_000_000_i128,
            &0_i128,
            &DEADLINE,
        );
        ta_sac.mint(&f.lp, &2_000_000_i128);
        tb_sac.mint(&f.lp, &2_000_000_i128);
        let shares = v2.add_liquidity(&f.lp, &2_000_000_i128, &2_000_000_i128, &0_i128, &DEADLINE);

        let v3_pool = env.register_contract(None, TestV3Pool);
        let v3 = TestV3PoolClient::new(env, &v3_pool);
        v3.setup(&ta.address, &tb.address, &0);

        f.registry.reg_v2(&v2_pool, &ta.address, &tb.address);
        f.registry
            .reg_cl(&v3_pool, &ta.address, &tb.address, &TEST_FEE_BPS);

        Pair {
            ta,
            tb,
            v2_pool,
            v2_lp: LpTokenClient::new(env, &v2_lp_addr),
            v3_pool,
            v3,
            shares,
        }
    }

    fn params(shares: i128) -> MigrateParams {
        MigrateParams {
            v2_shares: shares,
            min_a: 0,
            min_b: 0,
            tick_lower: -100,
            tick_upper: 100,
            range_width_ticks: 0,
            min_deposit_a: 0,
            min_deposit_b: 0,
            deadline: DEADLINE,
        }
    }

    fn event_emitted(env: &Env, name: &str) -> bool {
        env.events().all().iter().any(|event| {
            event.1.get(0).is_some_and(|topic| {
                Symbol::try_from_val(env, &topic) == Ok(Symbol::new(env, name))
            })
        })
    }

    /// Only `address` is authorized, and only for `fn_name` with no args.
    fn authorize_only(env: &Env, contract: &Address, address: &Address, fn_name: &'static str) {
        env.mock_auths(&[MockAuth {
            address,
            invoke: &MockAuthInvoke {
                contract,
                fn_name,
                args: ().into_val(env),
                sub_invokes: &[],
            },
        }]);
    }

    // ── one instance, many pairs ───────────────────────────────────────────────

    #[test]
    fn test_one_instance_migrates_two_different_pairs() {
        let env = Env::default();
        let f = fixture(&env);
        let p2 = add_pair(&env, &f);

        let first = f
            .migration
            .migrate(&f.lp, &f.v2_pool, &f.v3_pool, &params(f.lp_shares));
        let second = f
            .migration
            .migrate(&f.lp, &p2.v2_pool, &p2.v3_pool, &params(p2.shares));

        // Pair 1: 1M/1M withdrawn and deposited.
        assert_eq!(f.v2_lp.balance(&f.lp), 0);
        assert_eq!(
            (first.deposited_a, first.deposited_b),
            (1_000_000, 1_000_000)
        );
        assert_eq!(f.ta.balance(&f.v3_pool), 1_000_000);
        assert_eq!(f.tb.balance(&f.v3_pool), 1_000_000);
        assert_eq!(f.v3.position_count(), 1);

        // Pair 2: 2M/2M withdrawn and deposited into a different CL pool.
        assert_eq!(p2.v2_lp.balance(&f.lp), 0);
        assert_eq!(
            (second.deposited_a, second.deposited_b),
            (2_000_000, 2_000_000)
        );
        assert_eq!(p2.ta.balance(&p2.v3_pool), 2_000_000);
        assert_eq!(p2.tb.balance(&p2.v3_pool), 2_000_000);
        assert_eq!(p2.v3.position_count(), 1);

        // The pairs did not bleed into each other, and the helper holds nothing.
        assert_eq!(f.ta.balance(&p2.v3_pool), 0);
        assert_eq!(p2.ta.balance(&f.v3_pool), 0);
        assert_eq!(f.ta.balance(&f.migration_addr), 0);
        assert_eq!(p2.tb.balance(&f.migration_addr), 0);
    }

    #[test]
    fn test_migrate_respects_reversed_cl_token_order() {
        let env = Env::default();
        let f = build_fixture(&env, 1_000_000, 2_000_000, true, 0);

        let result = f
            .migration
            .migrate(&f.lp, &f.v2_pool, &f.v3_pool, &params(f.lp_shares));

        // CL token_a is the V2 pool's token_b.
        assert_eq!(
            (result.deposited_a, result.deposited_b),
            (1_142_857, 1_000_000)
        );
        assert_eq!(f.tb.balance(&f.v3_pool), 1_142_857);
        assert_eq!(f.ta.balance(&f.v3_pool), 1_000_000);
    }

    // ── factory attestation ────────────────────────────────────────────────────

    #[test]
    fn test_fake_v2_pool_is_rejected_and_no_funds_move() {
        let env = Env::default();
        let f = fixture(&env);
        let fake_v2 = env.register_contract(None, MockV2Pool);
        MockV2PoolClient::new(&env, &fake_v2).set_v2_tokens(&f.ta.address, &f.tb.address);

        let result = f
            .migration
            .try_migrate(&f.lp, &fake_v2, &f.v3_pool, &params(f.lp_shares));

        assert_eq!(result, Err(Ok(MigrationError::UnknownPool)));
        assert_eq!(f.v2_lp.balance(&f.lp), f.lp_shares);
        assert_eq!(f.ta.balance(&f.v3_pool), 0);
        assert_eq!(f.v3.position_count(), 0);
    }

    #[test]
    fn test_fake_cl_pool_is_rejected_and_no_funds_move() {
        let env = Env::default();
        let f = fixture(&env);
        let fake_v3 = env.register_contract(None, TestV3Pool);
        TestV3PoolClient::new(&env, &fake_v3).setup(&f.ta.address, &f.tb.address, &0);

        let result = f
            .migration
            .try_migrate(&f.lp, &f.v2_pool, &fake_v3, &params(f.lp_shares));

        assert_eq!(result, Err(Ok(MigrationError::UnknownPool)));
        assert_eq!(f.v2_lp.balance(&f.lp), f.lp_shares);
        assert_eq!(f.ta.balance(&fake_v3), 0);
        assert_eq!(TestV3PoolClient::new(&env, &fake_v3).position_count(), 0);
    }

    #[test]
    fn test_unregistered_cl_pool_of_the_right_pair_is_rejected() {
        let env = Env::default();
        let f = fixture(&env);
        // Same token pair and fee as the registered pool, different address.
        let lookalike = env.register_contract(None, TestV3Pool);
        TestV3PoolClient::new(&env, &lookalike).setup(&f.ta.address, &f.tb.address, &0);

        let result = f
            .migration
            .try_migrate(&f.lp, &f.v2_pool, &lookalike, &params(f.lp_shares));

        assert_eq!(result, Err(Ok(MigrationError::UnknownPool)));
        assert_eq!(f.v2_lp.balance(&f.lp), f.lp_shares);
        assert_eq!(f.ta.balance(&lookalike), 0);
    }

    #[test]
    fn test_cl_pool_registered_at_another_fee_tier_is_rejected() {
        let env = Env::default();
        let f = fixture(&env);
        // A registered CL pool, but the factory files it under a fee tier that
        // is not the one the pool reports.
        let other = env.register_contract(None, TestV3Pool);
        TestV3PoolClient::new(&env, &other).setup(&f.ta.address, &f.tb.address, &0);
        f.registry
            .reg_cl(&other, &f.ta.address, &f.tb.address, &100);

        let result = f
            .migration
            .try_migrate(&f.lp, &f.v2_pool, &other, &params(f.lp_shares));

        assert_eq!(result, Err(Ok(MigrationError::UnknownPool)));
    }

    #[test]
    fn test_pools_passed_in_the_wrong_slots_are_rejected() {
        let env = Env::default();
        let f = fixture(&env);

        let result = f
            .migration
            .try_migrate(&f.lp, &f.v3_pool, &f.v2_pool, &params(f.lp_shares));

        assert_eq!(result, Err(Ok(MigrationError::UnknownPool)));
        assert_eq!(f.v2_lp.balance(&f.lp), f.lp_shares);
    }

    #[test]
    fn test_registered_pools_of_different_pairs_are_a_token_mismatch() {
        let env = Env::default();
        let f = fixture(&env);
        let p2 = add_pair(&env, &f);

        let result = f
            .migration
            .try_migrate(&f.lp, &f.v2_pool, &p2.v3_pool, &params(f.lp_shares));

        assert_eq!(result, Err(Ok(MigrationError::TokenMismatch)));
        assert_eq!(f.v2_lp.balance(&f.lp), f.lp_shares);
    }

    #[test]
    fn test_migrate_without_a_factory_returns_factory_not_set() {
        let env = Env::default();
        let f = fixture(&env);
        env.as_contract(&f.migration_addr, || {
            env.storage().instance().remove(&DataKey::Factory);
        });

        let result = f
            .migration
            .try_migrate(&f.lp, &f.v2_pool, &f.v3_pool, &params(f.lp_shares));

        assert_eq!(result, Err(Ok(MigrationError::FactoryNotSet)));
    }

    // ── legacy single-pair entry points ────────────────────────────────────────

    #[test]
    fn test_migrate_default_still_works_on_a_pre_factory_instance() {
        let env = Env::default();
        let f = fixture(&env);
        // Emulate an instance deployed before #1150: stored pair, no factory.
        env.as_contract(&f.migration_addr, || {
            env.storage().instance().remove(&DataKey::Factory);
        });

        let result = f.migration.migrate_default(
            &f.lp,
            &f.lp_shares,
            &0,
            &0,
            &-100,
            &100,
            &0,
            &0,
            &0,
            &DEADLINE,
        );

        assert_eq!(
            (result.deposited_a, result.deposited_b),
            (1_000_000, 1_000_000)
        );
        assert_eq!(f.v2_lp.balance(&f.lp), 0);
        assert_eq!(
            f.migration.preview_range_default(&100, &200, &0),
            (100, 200)
        );
    }

    #[test]
    fn test_migrate_default_without_a_pair_returns_no_default_pair() {
        let env = Env::default();
        env.mock_all_auths();
        let admin = Address::generate(&env);
        let lp = Address::generate(&env);
        let factory = env.register_contract(None, MockFactory);
        let migration =
            MigrationContractClient::new(&env, &env.register_contract(None, MigrationContract));
        migration.initialize(&admin, &factory);

        let result =
            migration.try_migrate_default(&lp, &1, &0, &0, &100, &200, &0, &0, &0, &DEADLINE);

        assert_eq!(result, Err(Ok(MigrationError::NoDefaultPair)));
        assert_eq!(migration.get_default_pair(), None);
    }

    #[test]
    fn test_set_default_pair_stores_a_validated_pair() {
        let env = Env::default();
        let f = fixture(&env);
        let p2 = add_pair(&env, &f);
        assert_eq!(
            f.migration.get_default_pair(),
            Some((f.v2_pool.clone(), f.v3_pool.clone()))
        );

        f.migration.set_default_pair(&p2.v2_pool, &p2.v3_pool);

        assert_eq!(
            f.migration.get_default_pair(),
            Some((p2.v2_pool.clone(), p2.v3_pool.clone()))
        );
        assert!(event_emitted(&env, "default_pair_set"));

        let unknown = env.register_contract(None, MockV2Pool);
        assert_eq!(
            f.migration.try_set_default_pair(&unknown, &p2.v3_pool),
            Err(Ok(MigrationError::UnknownPool))
        );
    }

    // ── pause ──────────────────────────────────────────────────────────────────

    #[test]
    fn test_pause_blocks_migrate_but_not_previews() {
        let env = Env::default();
        let f = fixture(&env);
        assert!(!f.migration.is_paused());

        f.migration.pause();

        assert!(f.migration.is_paused());
        assert!(event_emitted(&env, "paused"));
        assert_eq!(
            f.migration
                .try_migrate(&f.lp, &f.v2_pool, &f.v3_pool, &params(f.lp_shares)),
            Err(Ok(MigrationError::Paused))
        );
        assert_eq!(
            f.migration.try_migrate_default(
                &f.lp,
                &f.lp_shares,
                &0,
                &0,
                &-100,
                &100,
                &0,
                &0,
                &0,
                &DEADLINE
            ),
            Err(Ok(MigrationError::Paused))
        );
        assert_eq!(f.v2_lp.balance(&f.lp), f.lp_shares);

        // Reads keep working while paused.
        assert_eq!(
            f.migration.preview_range(&f.v3_pool, &100, &200, &0),
            (100, 200)
        );
        assert_eq!(
            f.migration.preview_range_default(&100, &200, &0),
            (100, 200)
        );
        let preview =
            f.migration
                .preview_migration(&f.v2_pool, &f.v3_pool, &f.lp_shares, &-100, &100, &0);
        assert_eq!(preview.withdraw_a, 1_000_000);
    }

    #[test]
    fn test_unpause_restores_migrate() {
        let env = Env::default();
        let f = fixture(&env);
        f.migration.pause();
        f.migration.unpause();

        assert!(!f.migration.is_paused());
        assert!(event_emitted(&env, "unpaused"));
        let result = f
            .migration
            .migrate(&f.lp, &f.v2_pool, &f.v3_pool, &params(f.lp_shares));
        assert_eq!(result.deposited_a, 1_000_000);
    }

    #[test]
    fn test_pause_and_unpause_require_the_admin() {
        let env = Env::default();
        let f = fixture(&env);
        let attacker = Address::generate(&env);

        authorize_only(&env, &f.migration_addr, &attacker, "pause");
        assert!(f.migration.try_pause().is_err());
        assert!(!f.migration.is_paused());

        env.mock_all_auths();
        f.migration.pause();
        authorize_only(&env, &f.migration_addr, &attacker, "unpause");
        assert!(f.migration.try_unpause().is_err());
        assert!(f.migration.is_paused());
    }

    // ── upgrade and factory wiring are admin-only ──────────────────────────────

    #[test]
    fn test_upgrade_requires_the_admin() {
        let env = Env::default();
        let f = fixture(&env);
        let attacker = Address::generate(&env);
        let hash = BytesN::from_array(&env, &[7u8; 32]);

        env.mock_auths(&[MockAuth {
            address: &attacker,
            invoke: &MockAuthInvoke {
                contract: &f.migration_addr,
                fn_name: "upgrade",
                args: (hash.clone(),).into_val(&env),
                sub_invokes: &[],
            },
        }]);

        assert!(f.migration.try_upgrade(&hash).is_err());
        assert!(!event_emitted(&env, "upgraded"));
    }

    #[test]
    fn test_set_factory_is_admin_only_and_emits_an_event() {
        let env = Env::default();
        let f = fixture(&env);
        let new_factory = env.register_contract(None, MockFactory);
        let attacker = Address::generate(&env);

        env.mock_auths(&[MockAuth {
            address: &attacker,
            invoke: &MockAuthInvoke {
                contract: &f.migration_addr,
                fn_name: "set_factory",
                args: (new_factory.clone(),).into_val(&env),
                sub_invokes: &[],
            },
        }]);
        assert!(f.migration.try_set_factory(&new_factory).is_err());
        assert_eq!(f.migration.get_factory(), Some(f.factory.clone()));

        env.mock_all_auths();
        f.migration.set_factory(&new_factory);
        assert_eq!(f.migration.get_factory(), Some(new_factory));
        assert!(event_emitted(&env, "factory_set"));
    }

    // ── two-step admin rotation ────────────────────────────────────────────────

    #[test]
    fn test_admin_rotation_needs_the_nominees_acceptance() {
        let env = Env::default();
        let f = fixture(&env);
        let nominee = Address::generate(&env);
        assert_eq!(f.migration.get_admin(), Some(f.admin.clone()));

        f.migration.propose_admin(&nominee);

        // Nominated, not yet in charge.
        assert_eq!(f.migration.get_admin(), Some(f.admin.clone()));
        assert_eq!(f.migration.get_pending_admin(), Some(nominee.clone()));
        assert!(event_emitted(&env, "admin_nominated"));

        f.migration.accept_admin(&nominee);

        assert_eq!(f.migration.get_admin(), Some(nominee));
        assert_eq!(f.migration.get_pending_admin(), None);
        assert!(event_emitted(&env, "admin_changed"));
    }

    #[test]
    fn test_accept_admin_rejects_a_stranger_and_a_missing_nomination() {
        let env = Env::default();
        let f = fixture(&env);
        let nominee = Address::generate(&env);
        let stranger = Address::generate(&env);

        assert_eq!(
            f.migration.try_accept_admin(&nominee),
            Err(Ok(MigrationError::NoPendingAdmin))
        );

        f.migration.propose_admin(&nominee);
        assert_eq!(
            f.migration.try_accept_admin(&stranger),
            Err(Ok(MigrationError::WrongAdmin))
        );
        assert_eq!(f.migration.get_admin(), Some(f.admin.clone()));
    }

    #[test]
    fn test_old_admin_loses_its_powers_after_rotation() {
        let env = Env::default();
        let f = fixture(&env);
        let nominee = Address::generate(&env);
        f.migration.propose_admin(&nominee);
        f.migration.accept_admin(&nominee);

        // Only the previous admin signs from here on.
        authorize_only(&env, &f.migration_addr, &f.admin, "pause");
        assert!(f.migration.try_pause().is_err());

        // The new admin can pause.
        authorize_only(&env, &f.migration_addr, &nominee, "pause");
        f.migration.pause();
        assert!(f.migration.is_paused());
    }

    #[test]
    fn test_admin_calls_before_initialize_return_not_initialized() {
        let env = Env::default();
        env.mock_all_auths();
        let migration =
            MigrationContractClient::new(&env, &env.register_contract(None, MigrationContract));

        assert_eq!(
            migration.try_pause(),
            Err(Ok(MigrationError::NotInitialized))
        );
        assert_eq!(
            migration.try_propose_admin(&Address::generate(&env)),
            Err(Ok(MigrationError::NotInitialized))
        );
        assert_eq!(migration.get_admin(), None);
    }

    // ── preview_range with a pool argument ─────────────────────────────────────

    #[test]
    fn test_preview_range_validates_its_pool_argument() {
        let env = Env::default();
        let f = fixture(&env);
        let fake = env.register_contract(None, TestV3Pool);
        TestV3PoolClient::new(&env, &fake).setup(&f.ta.address, &f.tb.address, &0);

        assert_eq!(
            f.migration
                .preview_range(&f.v3_pool, &i32::MIN, &i32::MAX, &50),
            (-50, 50)
        );
        assert_eq!(
            f.migration.try_preview_range(&fake, &100, &200, &0),
            Err(Ok(MigrationError::UnknownPool))
        );
    }

    #[test]
    fn test_preview_range_follows_each_pools_own_tick() {
        let env = Env::default();
        let f = fixture(&env);
        let p2 = add_pair(&env, &f);
        p2.v3.set_current_tick(&4_000);

        assert_eq!(
            f.migration
                .preview_range(&f.v3_pool, &i32::MIN, &i32::MAX, &50),
            (-50, 50)
        );
        assert_eq!(
            f.migration
                .preview_range(&p2.v3_pool, &i32::MIN, &i32::MAX, &50),
            (3_950, 4_050)
        );
    }

    // ── preview_migration ──────────────────────────────────────────────────────

    #[test]
    fn test_preview_migration_in_range_sizes_the_position_by_the_binding_leg() {
        let env = Env::default();
        let f = fixture(&env);

        let preview =
            f.migration
                .preview_migration(&f.v2_pool, &f.v3_pool, &f.lp_shares, &-100, &100, &0);

        // Harness quote: in range (L, 2L). Token B binds: L = 1M / 2.
        assert_eq!(
            preview,
            MigrationPreview {
                withdraw_a: 1_000_000,
                withdraw_b: 1_000_000,
                deposit_a: 500_000,
                deposit_b: 1_000_000,
                liquidity: 500_000,
                leftover_a: 500_000,
                leftover_b: 0,
                tick_lower: -100,
                tick_upper: 100,
            }
        );
    }

    #[test]
    fn test_preview_migration_below_range_deposits_only_token_a() {
        let env = Env::default();
        let f = fixture(&env);

        let preview =
            f.migration
                .preview_migration(&f.v2_pool, &f.v3_pool, &f.lp_shares, &100, &200, &0);

        assert_eq!((preview.deposit_a, preview.deposit_b), (1_000_000, 0));
        assert_eq!((preview.leftover_a, preview.leftover_b), (0, 1_000_000));
        assert_eq!(preview.liquidity, 1_000_000);
    }

    #[test]
    fn test_preview_migration_above_range_deposits_only_token_b() {
        let env = Env::default();
        let f = fixture(&env);

        let preview =
            f.migration
                .preview_migration(&f.v2_pool, &f.v3_pool, &f.lp_shares, &-200, &-100, &0);

        assert_eq!((preview.deposit_a, preview.deposit_b), (0, 1_000_000));
        assert_eq!((preview.leftover_a, preview.leftover_b), (1_000_000, 0));
        assert_eq!(preview.liquidity, 1_000_000);
    }

    #[test]
    fn test_preview_migration_auto_range_matches_preview_range() {
        let env = Env::default();
        let f = fixture(&env);

        let preview = f.migration.preview_migration(
            &f.v2_pool,
            &f.v3_pool,
            &f.lp_shares,
            &i32::MIN,
            &i32::MAX,
            &250,
        );

        assert_eq!((preview.tick_lower, preview.tick_upper), (-250, 250));
        assert_eq!(
            f.migration
                .preview_range(&f.v3_pool, &i32::MIN, &i32::MAX, &250),
            (-250, 250)
        );
    }

    #[test]
    fn test_preview_migration_reports_amounts_in_cl_token_order() {
        let env = Env::default();
        let f = build_fixture(&env, 1_000_000, 2_000_000, true, 0);

        let preview =
            f.migration
                .preview_migration(&f.v2_pool, &f.v3_pool, &f.lp_shares, &-100, &100, &0);

        // CL token_a is the V2 pool's token_b.
        assert_eq!(
            (preview.withdraw_a, preview.withdraw_b),
            (1_142_857, 1_000_000)
        );
    }

    #[test]
    fn test_preview_migration_scales_withdrawal_with_shares() {
        let env = Env::default();
        let f = fixture(&env);

        let preview = f.migration.preview_migration(
            &f.v2_pool,
            &f.v3_pool,
            &(f.lp_shares / 4),
            &-100,
            &100,
            &0,
        );

        assert_eq!((preview.withdraw_a, preview.withdraw_b), (250_000, 250_000));
    }

    #[test]
    fn test_preview_migration_rejects_bad_input() {
        let env = Env::default();
        let f = fixture(&env);
        let fake = env.register_contract(None, MockV2Pool);
        MockV2PoolClient::new(&env, &fake).set_v2_tokens(&f.ta.address, &f.tb.address);

        assert_eq!(
            f.migration
                .try_preview_migration(&f.v2_pool, &f.v3_pool, &0, &-100, &100, &0),
            Err(Ok(MigrationError::ZeroShares))
        );
        assert_eq!(
            f.migration.try_preview_migration(
                &f.v2_pool,
                &f.v3_pool,
                &100_000_000,
                &-100,
                &100,
                &0
            ),
            Err(Ok(MigrationError::ZeroShares))
        );
        assert_eq!(
            f.migration
                .try_preview_migration(&fake, &f.v3_pool, &1_000, &-100, &100, &0),
            Err(Ok(MigrationError::UnknownPool))
        );
        assert_eq!(
            f.migration
                .try_preview_migration(&f.v2_pool, &f.v3_pool, &1_000, &200, &100, &0),
            Err(Ok(MigrationError::InvalidRange))
        );
    }

    #[test]
    fn test_preview_migration_does_not_move_funds_or_burn_shares() {
        let env = Env::default();
        let f = fixture(&env);

        let _ =
            f.migration
                .preview_migration(&f.v2_pool, &f.v3_pool, &f.lp_shares, &-100, &100, &0);

        assert_eq!(f.v2_lp.balance(&f.lp), f.lp_shares);
        assert_eq!(f.ta.balance(&f.v3_pool), 0);
        assert_eq!(f.v3.position_count(), 0);
    }
}
