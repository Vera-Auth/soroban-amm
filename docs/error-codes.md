# Error codes


This document provides a complete reference for every error code emitted by the
Soroban AMM contracts across all contract crates in `contracts/`. For each code you will find:

- **Numeric code** – the on-chain discriminant embedded in the XDR `InvokeHostFunctionResult` when the contract returns that error.
- **Symbol** – the symbolic name in Rust source and decoded XDR.
- **Cause** – what precondition guard was violated in the contract code.
- **Remedy** – actionable instructions for the caller to recover.

Use the numeric code when parsing RPC responses or writing off-chain tooling.

## OracleAggregator (`contracts/oracle_aggregator`)

Defined in [contracts/oracle_aggregator/src/lib.rs](../contracts/oracle_aggregator/src/lib.rs) as `OracleError`.

| Code | Symbol | Cause | Remedy |
|------|--------|-------|--------|
| 1 | `AlreadyInitialized` | `initialize` was called on an already-initialized aggregator. | Deploy a new aggregator contract. |
| 2 | `NotInitialized` | A function was called before `initialize`. | Call `initialize` first. |
| 3 | `NotAdmin` | The caller did not match the stored admin address. | Use the correct admin keypair. |
| 4 | `SourceAlreadyRegistered` | `register_source` was called with an address already in the registry. | Remove and re-register, or use a different address. |
| 5 | `SourceNotFound` | `remove_source` or `set_source_weight` referenced an unknown address. | Check `list_sources()` for registered addresses. |
| 6 | `InsufficientSources` | Fewer than `MIN_VALID_SOURCES` fresh, agreeing sources were available, or `get_price` was called with insufficient confidence. | Register more sources or widen the deviation band. |
| 7 | `InvalidStaleness` | `max_staleness_seconds` was zero. | Use a positive value. |
| 8 | `InvalidDeviation` | `max_deviation_bps` was zero or exceeded `BPS_DENOMINATOR` (10 000). | Use a value in `1..=10_000`. |
| 9 | `InvalidWeight` | A source weight was zero or exceeded `MAX_SOURCE_WEIGHT` (100 000). | Use a value in `1..=100_000`. |
| 10 | `WeightFloorNotMet` | The total agreeing weight was below `MIN_AGREEING_WEIGHT` (20 000). | Increase individual source weights or register more sources. |
| 11 | `NoPendingAdmin` | `accept_admin` was called when no admin transfer is in progress. | Call `propose_admin` first to nominate a successor. |
| 12 | `WrongAdmin` | `accept_admin` was called by an address that does not match the pending nominee. | Have the correct address (the one passed to `propose_admin`) call `accept_admin`. |
| 13 | `Paused` | The aggregator has been paused. | Wait for the admin to call `unpause`. |

> **ABI change (#689):** `AggregatedPrice.confidence` is now the **summed weight**
> of agreeing sources (not a raw count). A source with weight 10 000 contributes
> 10 000 to confidence, not 1. This is a breaking change for off-chain consumers
> that interpreted `confidence` as a source count.

## Amm (`contracts/amm`)


Defined in [contracts/amm/src/lib.rs](../contracts/amm/src/lib.rs) as `AmmError`.

| Code | Symbol | Cause | Remedy |
|------|--------|-------|--------|
| 1 | `AlreadyInitialized` | `initialize` (or `initialize_with_flash_loan_fee`) was called on a pool that has already been set up. The presence of `DataKey::TokenA` in instance storage is the guard. | Deploy a fresh pool contract instead of re-initializing. |
| 2 | `InvalidFeeBps` | A swap fee was outside `[0, 10 000)` bps (0 to 9,999 bps), a flash loan fee was outside `[0, 10 000]` bps, `protocol_fee_bps` exceeded `fee_bps`, `new_fee_bps` was below the current `protocol_fee_bps`, or `get_amount_in`/`get_amount_out` was called on a pool with `fee_bps >= 10_000`. | Ensure `0 ≤ protocol_fee_bps ≤ fee_bps < 10 000`. |
| 3 | `InsufficientShares` | The LP share burn amount in `remove_liquidity` exceeded the provider's actual LP token balance. | Query `shares_of(provider)` first and cap the burn amount to that value. |
| 4 | `DeadlineExceeded` | The `deadline` ledger timestamp passed before the transaction was included. | Re-submit with `deadline = current_ledger_timestamp + buffer`. |
| 5 | `SlippageExceeded` | For swaps: `amount_out < min_out` or `required_in > max_in`. For `add_liquidity`: minted shares < `min_shares`. For `remove_liquidity`: `out_a < min_a` or `out_b < min_b`. | Widen slippage bounds or use `simulate_swap` / `get_amount_in` to recalculate before submitting. |
| 6 | `Paused` | The pool has been administratively paused via `pause()`. All state-mutating functions (swap, add/remove liquidity, flash loan) reject calls while paused. | Wait for the admin (or governance) to call `unpause()`. |
| 7 | `Unauthorized` | A caller passed an `admin` address that does not match the stored admin. | Use the correct admin keypair. The current admin can be read with `get_info().admin`. |
| 8 | `ZeroAmount` | An `amount_*` argument was zero or negative (`amount <= 0`). | Pass a strictly positive value. |
| 9 | `InvalidToken` | `token_in`, `token_out`, or `token` did not match either of the two pool tokens. | Use `get_info()` to discover valid token addresses. |
| 10 | `EmptyPool` | A swap or `price_ratio` was attempted on a pool where at least one reserve is zero. | Add liquidity before trading. |
| 11 | `InsufficientLiquidity` | Either `amount_out ≥ reserve_out` (would drain the pool), or `reserve < amount` for a flash loan, or the flash loan receiver did not repay (`balance_after < balance_before + fee`). | Reduce the trade/loan size, or ensure the flash loan receiver repays the principal plus fee within the callback. |
| 12 | `NoPendingAdmin` | `accept_admin` was called when no admin transfer is in progress. | Call `propose_admin` first to nominate a successor. |
| 13 | `WrongAdmin` | `accept_admin` was called by an address that does not match the pending nominee. | Have the correct address (the one passed to `propose_admin`) call `accept_admin`. |
| 14 | `Reentrant` | A reentrant call was made into any fund-moving entry point — `swap`, `swap_exact_out`, `swap_fot`, `add_liquidity`, `add_liquidity_fot`, `remove_liquidity`, `remove_liquidity_one_sided`, `flash_loan`, `withdraw_protocol_fees`, or `emergency_withdraw` — while the pool-wide reentrancy lock (`DataKey::Locked`) was already held. Most commonly this is a flash-loan receiver's `on_flash_loan` callback calling back into the pool, or a malicious/fee-on-transfer token's `transfer` implementation calling back into the pool mid-swap. | Do not call any of the functions above from inside `on_flash_loan` or a token's own `transfer`/`transfer_from`. Perform all swaps and liquidity operations *before* or *after* the flash loan, not during the callback. Query `is_locked()` (formerly `flash_loan_locked()`, kept as a deprecated alias) to check whether the lock is currently held. If the lock is ever stranded, the admin (or, when a multisig is configured, `quorum` of its signers) can clear it with `force_unlock`. |
| 15 | `CircuitBreaker` | Legacy error code kept for ABI compatibility. Since #1094, price-deviation breaches during `swap`, `swap_exact_out`, `swap_fot`, `remove_liquidity_one_sided`, `flash_loan`, or `trip_circuit_breaker` durably pause the pool (`Paused = true`, `CircuitBreakerTriggeredAt = now`), emit `circuit_break`, and return a zero fill (`Ok(0)` or `Ok((0, 0))`) so the pause state is not rolled back; subsequent mutating calls return `Paused` (`6`). | Wait for the cooldown period to elapse (default 600 s) and call `try_circuit_breaker_recovery`, or have the admin/governance call `unpause`. |
| 16 | `FotSlippage` | A fee-on-transfer token deducted fees resulting in fewer tokens received than the `min_received` guard (`received < min_received`). | Widen `min_received` threshold or trade using standard non-FoT tokens. |
| 17 | `OracleDeviationExceeded` | Spot price deviated beyond configured oracle tolerance (`\|spot - oracle\| > max_deviation`). | Retry when spot price or oracle price stabilizes within configured tolerance. |
| 18 | `FlashLoanRepaymentFailed` | Receiver contract failed to return borrowed tokens plus fee (`balance_after < balance_before + fee`). | Ensure `on_flash_loan` callback repays principal and fee in full. |
| 19 | `AlreadyExecuted` | Emergency withdrawal multisig proposal was already executed (`proposal.executed == true`). | No action required; proposal has already been executed. |
| 20 | `ProposalExpired` | Emergency withdrawal multisig proposal exceeded its validity window (`now > proposal.expires_at`). | Submit a new emergency withdrawal proposal. |
| 21 | `NotInitialized` | A function that reads pool configuration (tokens, LP token, admin, fee settings) was called before `initialize`. Covers admin setters, liquidity, swaps, quotes, and the `get_info` / `get_fee_info` / `shares_of` views. | Call `initialize` first. |
| 22 | `PoolDrained` | The pool has entered the terminal `Drained` state after an `emergency_withdraw` or `exec_multisig_emergency_wd` (#1095). All liquidity, swap, flash-loan, fee-withdrawal, unpause, recovery, and repeat emergency-withdrawal entrypoints are permanently disabled so pre-existing LP tokens cannot drain subsequent deposits. | Deploy a new pool contract; a drained pool is permanently retired. |
| 23 | `QuorumNotMet` | `exec_multisig_emergency_wd` was called while the number of collected signer approvals was below `MultisigQuorum` (#1095). | Collect additional signer approvals via `approve_emergency_withdraw` until `approvals.len() >= quorum`. |
| 24 | `NoPendingProposal` | `exec_multisig_emergency_wd` was called when no multisig emergency withdrawal proposal has been created (#1095). | Have a multisig signer call `propose_emergency_withdraw` first. |

---

## AmmSdk (`contracts/amm-sdk`)

Defined in [contracts/amm-sdk/src/types.rs](../contracts/amm-sdk/src/types.rs) as `SdkAmmError`.

| Code | Symbol | Cause | Remedy |
|------|--------|-------|--------|
| 1 | `AlreadyInitialized` | `initialize` called on a pool that is already set up. | Deploy a fresh pool contract instead. |
| 2 | `InvalidFeeBps` | Swap fee outside `[0, 10 000)` bps, flash loan fee outside `[0, 10 000]`, or protocol fee > swap fee. | Use a value in the accepted range. |
| 3 | `InsufficientShares` | LP burn amount exceeds caller's balance. | Reduce `shares` to ≤ `shares_of(provider)`. |
| 4 | `DeadlineExceeded` | `deadline` ledger timestamp already passed. | Re-submit with a future deadline. |
| 5 | `SlippageExceeded` | Output or input violated the slippage guard. | Widen `min_out` / `max_in` or retry later. |
| 6 | `Paused` | Pool is administratively paused. | Wait for admin to call `unpause`. |
| 7 | `Unauthorized` | Caller does not match stored admin. | Use correct admin keypair. |
| 8 | `ZeroAmount` | Amount argument is zero or negative. | Pass a positive value. |
| 9 | `InvalidToken` | `token_in`/`token_out` is not a pool token. | Use `pool.get_info()` to discover valid tokens. |
| 10 | `EmptyPool` | One or both reserves are zero. | Add liquidity before trading. |
| 11 | `InsufficientLiquidity` | Output ≥ reserve or flash loan not repaid. | Reduce trade size or ensure repayment. |
| 12 | `NoPendingAdmin` | `accept_admin` called without a prior `propose_admin`. | Call `propose_admin` first. |
| 13 | `WrongAdmin` | `accept_admin` caller ≠ pending nominee. | Have correct address call `accept_admin`. |
| 14 | `Reentrant` | Reentrant call detected during flash loan callback. | Do not call pool functions from `on_flash_loan`. |
| 15 | `CircuitBreaker` | Legacy circuit-breaker code (breaches now durably pause and return zero fill; see `AmmError::CircuitBreaker`). | Wait for cooldown or governance action. |
| 16 | `FotSlippage` | Fee-on-transfer token deducted more than `min_received`. | Widen `min_received` or use non-FoT token. |
| 17 | `OracleDeviationExceeded` | Spot price deviated beyond oracle tolerance. | Retry when oracle price stabilises. |
| 18 | `FlashLoanRepaymentFailed` | Receiver did not repay borrowed amounts + fees. | Ensure `on_flash_loan` repays in full. |
| 19 | `AlreadyExecuted` | Multisig emergency withdrawal was already executed. | No action — proposal already carried out. |
| 20 | `ProposalExpired` | Multisig emergency withdrawal proposal has expired. | Submit a new proposal. |
| 21 | `NotInitialized` | Pool configuration was read before `initialize`. | Call `initialize` first. |
| 22 | `PoolDrained` | Pool entered terminal `Drained` state after emergency withdrawal (#1095). | Deploy a new pool; drained pools are permanently retired. |
| 23 | `QuorumNotMet` | Multisig emergency withdrawal approvals are below `MultisigQuorum` (#1095). | Collect enough signer approvals before executing. |
| 24 | `NoPendingProposal` | `exec_multisig_emergency_wd` called with no active multisig proposal (#1095). | Call `propose_emergency_withdraw` first. |

---

## BatchAuction (`contracts/batch_auction`)

Defined in [contracts/batch_auction/src/lib.rs](../contracts/batch_auction/src/lib.rs) as `AuctionError`.

| Code | Symbol | Cause | Remedy |
|------|--------|-------|--------|
| 1 | `AlreadyInitialized` | `initialize` called on an already configured auction contract instance. | Deploy a fresh batch auction contract instance. |
| 2 | `Unauthorized` | Action invoked by caller other than the registered admin or keeper. | Call method using the stored admin or keeper credentials. |
| 3 | `OrderNotFound` | Order lookup failed for the provided `order_id`. | Query active orders using `get_order` to verify ID before cancelling or modifying. |
| 4 | `BatchWindowOpen` | Settlement or clearing attempted while the batch submission window is still open. | Wait until batch submission deadline elapses before executing settlement. |
| 5 | `NoOrders` | Batch clearing executed with zero orders in the current batch queue. | Submit orders to the batch before initiating clearing. |
| 6 | `ZeroAmount` | Order amount supplied was zero or negative (`amount <= 0`). | Provide a strictly positive order amount. |
| 7 | `DeadlineExceeded` | Order submission timestamp exceeded order deadline. | Submit order with a future deadline. |
| 8 | `BatchFull` | Number of orders in batch reached `max_orders` capacity limit. | Wait for the next batch window or increase `max_orders` configuration. |
| 9 | `InvalidMaxOrders` | `max_orders` set to 0 or above `MAX_ORDERS_CEILING` (200). | Configure `max_orders` within valid `1..=200` range. |
| 10 | `InvalidPoolTokenPair` | Specified `(token_in, token_out)` pair does not match the auction pool's token pair. | Pass token addresses matching the configured pool tokens. |
| 11 | `TransferFailed` | Token transfer to/from trader failed during deposit or payout. | Verify token balance and allowance before placing order. |
| 12 | `NoPendingAdmin` | `accept_admin` called without a prior `propose_admin`. | Call `propose_admin` first to set nominee. |
| 13 | `WrongAdmin` | `accept_admin` called by address other than the nominated pending admin. | Invoke `accept_admin` from the nominee address. |
| 14 | `UnknownVenue` | `pool`/`alt_pool` is neither on the admin allowlist (`add_venue`) nor attested to by the configured factory. | Deploy through the factory, or ask the admin to `add_venue` it. |
| 15 | `VenueRemoved` | Internal: an order's venue was removed from the registry between submission and settlement. | No action — the order is refunded automatically at settlement. |
| 16 | `DeadlineTooFar` | `deadline` is further in the future than `MAX_ORDER_LIFETIME_SECS` (7 days). | Submit with a nearer deadline. |
| 17 | `OrderNotExpired` | `expire_order` called on an order whose deadline has not passed yet. | Wait until the order's deadline, or use `cancel_order` instead. |
| 18 | `NothingToClaim` | `claim_refund` called for an order with no claimable balance on record. | No action — nothing was stranded for this order. |
| 19 | `TooManyVenues` | `add_venue` would push the persistent venue registry past `MAX_VENUES`. | Remove a stale venue with `remove_venue` before adding another. |

---

## BatchRouter (`contracts/batch_router`)

Defined in [contracts/batch_router/src/lib.rs](../contracts/batch_router/src/lib.rs) as `BatchRouterError`.

| Code | Symbol | Cause | Remedy |
|------|--------|-------|--------|
| 1 | `AlreadyInitialized` | `initialize` called on a batch router that already has a `DataKey::Factory` set. | Deploy a fresh batch router contract instance. |
| 2 | `EmptyBatch` | `execute_batch`, `simulate_batch`, or `validate_batch` called with an empty operations array. | Provide at least one batch operation. |
| 3 | `BatchTooLarge` | `ops.len()` exceeds `MAX_BATCH_OPS` (200), the ceiling returned by `max_batch_ops`. | Split the batch into multiple calls, each within the ceiling. |
| 4 | `DeadlineExpired` | `env.ledger().timestamp() > deadline` at batch time. | Re-submit the batch with a future deadline. |
| 5 | `InvalidAmount` | A `Swap`/`AddLiquidity`/`RemoveLiquidity` op carried a non-positive `amount_in`/`amount_a`/`amount_b`/`shares`. | Ensure every op amount is strictly positive. |
| 6 | `PoolNotFound` | An AMM op named a pool the factory does not recognize (`get_pool_tokens` returned `None`), or a CL op named a pool for which `is_cl_pool` is `false`. | Target only pools registered with the batch router's factory, and match `pool_kind` to the venue. |
| 7 | `SlippageExceeded` | The simulated or executed output/shares fell below the op's `min_out`/`min_shares`/`min_a`/`min_b` guard. | Loosen the slippage guard or resubmit against fresher pool state. |
| 8 | `PoolPaused` | The target AMM or CL pool is paused, so `execute_batch` would reject the op. Returned by `simulate_batch`/`validate_batch` instead of a raw pool error. | Wait for the pool admin to unpause, or drop the op. |
| 9 | `UnsimulatableChain` | `simulate_batch` reached a second concentrated-liquidity op on a pool an earlier op already touched, so its local snapshot is stale. | Split the batch, or place the CL op first and re-quote afterwards. |
| 10 | `InsufficientLiquidity` | The AMM swap's output would meet or exceed the output reserve (mirrors `amm::swap`'s guard). Not reachable through valid constant-product math, but simulated for parity. | Reduce the swap size. |
| 11 | `InsufficientShares` | A first deposit would mint no more than `MINIMUM_LIQUIDITY` (1 000) shares, all of which are permanently locked, leaving the provider with none (mirrors `amm::add_liquidity`). | Deposit enough that `isqrt(amount_a * amount_b) > 1 000`. |

---

## ConcentratedLiquidity (`contracts/concentrated_liquidity`)

Defined in [contracts/concentrated_liquidity/src/lib.rs](../contracts/concentrated_liquidity/src/lib.rs) as `ClError`.

| Code | Symbol | Cause | Remedy |
|------|--------|-------|--------|
| 1 | `AlreadyInitialized` | `initialize` called on an already-configured pool. | Deploy a new pool. |
| 2 | `TokensMustDiffer` | `token_a == token_b` during initialization. | Use two distinct token addresses. |
| 3 | `InvalidFeeBps` | Fee outside `[0, 10 000]` bps. | Use a value in the accepted range. |
| 4 | `TickOutOfRange` | A tick value was outside `[-MAX_TICK, MAX_TICK]` (±887 272). `lower_tick` must also be < `upper_tick`. | Keep ticks inside the valid range and ensure lower < upper. |
| 5 | `ZeroAmounts` | Both `amount_a_desired` and `amount_b_desired` were zero or negative. | Provide at least one positive desired amount. |
| 6 | `SlippageExceeded` | `amount_a < min_a` or `amount_b < min_b` on `mint_position`, or output below `min_out` on `swap`. | Widen slippage bounds. |
| 7 | `ZeroLiquidity` | Computed liquidity for a new position was zero (amounts too small relative to the price range). | Increase the deposit amounts or narrow the tick range. |
| 8 | `InsufficientLiquidity` | `burn_position` requested more liquidity than the position holds, or swap output would exceed available liquidity. | Burn ≤ `position.liquidity`; reduce swap size. |
| 9 | `PositionNotFound` | `collect_fees`, `burn_position`, or `collect_all` referenced a position `(owner, lower_tick, upper_tick)` that does not exist. | Verify the position exists with `get_position` before operating on it. |
| 10 | `DeadlineExpired` | The `deadline` timestamp passed before execution. | Re-submit with a future deadline. |
| 11 | `Paused` | Pool is paused. | Wait for admin to unpause. |
| 12 | `Unauthorized` | Admin mismatch. | Use the stored admin address. |
| 13 | `TickNotAligned` | A tick was not a multiple of `tick_spacing`. | Round ticks to the nearest multiple of `tick_spacing`. |
| 14 | `InvalidTickSpacing` | `tick_spacing ≤ 0` during initialization. | Use a positive tick spacing (common values: 1, 10, 60, 200). |
| 15 | `TickNotInitialized` | A swap crossed into a tick that has no liquidity (never been used by any position). | Ensure positions cover the full swap range, or use a smaller swap amount. |
| 16 | `InvalidToken` | `token_in` is not `token_a` or `token_b`. | Check `get_info()` for valid token addresses. |
| 17 | `RangeOrderInRange` | `place_range_order` called when current tick is inside `[lower_tick, upper_tick)`. | Select a tick range strictly above or below current tick. |
| 18 | OracleDeviationExceeded | Spot price deviated beyond configured oracle tolerance during swap, or configured oracle returned non-positive price or confidence 0. | Wait for oracle price and confidence to stabilize or adjust tolerance. |
| 19 | `NftNotConfigured` | Tokenization attempted when no NFT contract address is set in storage. | Call `set_nft_contract` with a valid position NFT contract. |
| 20 | `NotNftOwner` | Caller is not current owner of position NFT. | Call method from NFT owner account. |
| 21 | `NftContractChangeBlocked` | Admin attempted NFT contract change while tokenized positions exist. | Untokenize/burn active position NFTs before changing contract. |
| 22 | `RangeOrderExists` | Range order already active on specified range for caller. | Withdraw existing range order before placing a new one. |
| 23 | `ExactOutNotFullyFilled` | `swap_exact_out` or `quote_exact_out` (#696) could not fill the requested `amount_out` in full before running out of initialized ticks or hitting `sqrt_price_limit_x96`. Exact-out has no meaningful partial fill. | Reduce `amount_out`, widen `sqrt_price_limit_x96`, or add liquidity to the range being traded against. |
| 24 | `NotInitialized` | A function that depends on pool state (tokens, admin, current tick) was called before `initialize`. This covers the admin setters, every liquidity/swap/quote entrypoint, and the `current_tick`, `get_tokens`, and `fee_bps` views. | Call `initialize` first. |
| 25 | `MathOverflow` | A position amount or liquidity figure computed by `mint_position`, `mint_position_single_token`, `quote_single_token_deposit`, `modify_position`, `burn_position` or `quote_position` does not fit in an `i128`, or an intermediate of its 256-bit evaluation does not fit in a `u128`. Reported instead of a truncated value (#963). | Use a smaller amount or liquidity, or a wider tick range. |
| 26 | `ObservationTooOld` | `observe(seconds_ago)` requested a timestamp earlier than the oldest observation retained in the persistent oracle ring buffer (or `seconds_ago > now`) (#1101). | Query a smaller `seconds_ago` within the retained ring-buffer window, or have the admin grow the ring capacity via `set_oracle_cardinality`. |
| 27 | `InvalidOracleCardinality` | `set_oracle_cardinality` was called with `0`, a value smaller than the current `cardinality`, or a value exceeding `MAX_ORACLE_CARDINALITY` (`2048`) (#1101). | Pass a cardinality in `[current_cardinality, 2048]`. |

`swap_exact_out(env, sender, zero_for_one, amount_out, sqrt_price_limit_x96,
max_amount_in, deadline)` (#696) is the mirror of `swap`: it fixes the
*output* amount instead of the input, reverting with `SlippageExceeded` if
the required input exceeds `max_amount_in` and with
`ExactOutNotFullyFilled` on a partial fill. `quote_exact_out(env,
zero_for_one, amount_out, sqrt_price_limit_x96)` is a read-only simulation
sharing the same tick-walking core, so it can never disagree with what
`swap_exact_out` actually charges on the same pool state.

---

## DexAggregator (`contracts/dex_aggregator`)

Defined in [contracts/dex_aggregator/src/lib.rs](../contracts/dex_aggregator/src/lib.rs) as `AggregatorError`.

| Code | Symbol | Cause | Remedy |
|------|--------|-------|--------|
| 1 | `NoRouteFound` | No valid liquidity route connects `token_in` and `token_out` across registered pools. | Provide connected intermediate pools or update route path. |
| 2 | `SlippageExceeded` | Total output across aggregated swap steps fell below `min_amount_out`. | Widen slippage tolerance or update quote before submitting. |
| 3 | `UnregisteredPool` | A route hop references a pool that is not registered with the factory. | Only route through pools registered via the factory. |
| 4 | `InvalidMaxHops` | `set_max_hops` called with `0`. | Pass a positive hop count. |
| 5 | `TooManyRoutingTokens` | `set_routing_tokens` called with more than `MAX_ROUTING_TOKENS` addresses. | Reduce the routing token list size. |
| 6 | `NotInitialized` | An admin setter, quote, swap, or `pause`/`unpause` entrypoint was called before `initialize` (admin/factory unset). | Call `initialize` first. |
| 7 | `Paused` | A state-mutating entrypoint (`set_max_hops`, `register_cl_pool`, `set_routing_tokens`, `remove_cl_pool`, `execute_route`, `swap_best`) was called while the aggregator is paused. Quotes (`find_best_route`, `get_quote`, `is_price_within_tolerance`) stay callable. | Wait for the admin to call `unpause`; check `is_paused` first. |
| 8 | `Unauthorized` | `pause` or `unpause` was called with an `admin` address that does not match the stored admin. | Pass the stored admin address and sign with its key. |

---

## TwalConsumer (`contracts/twal_consumer`)

Defined in [contracts/twal_consumer/src/lib.rs](../contracts/twal_consumer/src/lib.rs) as `TwalError`.

| Code | Symbol | Cause | Remedy |
|------|--------|-------|--------|
| 1 | `AlreadyInitialized` | `initialize` called twice. | Deploy a new consumer contract. |
| 2 | `NotInitialized` | `get_keeper` (and thus keeper-gated calls) invoked before `initialize`. | Call `initialize` first. |
| 3 | `ZeroWindow` | `window_seconds` was `0` on `get_twal_liquidity` or a batch read. | Pass a positive window. |
| 4 | `InsufficientHistory` | The ledger has not yet advanced past `window_seconds`, so `now - window` would underflow. | Wait until `ledger_timestamp >= window_seconds`, or use a shorter window. |
| 5 | `NoSnapshotFound` | No `save_snapshot`/`save_cl_snapshot` was recorded at exactly `now - window_seconds`. | Snapshot on a fixed cadence that lines up with the windows you intend to query. |
| 6 | `ElapsedZero` | The pool's own timestamp did not advance between the snapshot and now (no liquidity-changing operation occurred). | Query a window in which the pool actually transacted. |
| 7 | `TooManyTrackedPools` | `add_tracked_pool`, or implicit registration inside `save_snapshot`/`save_cl_snapshot`, would grow the tracked set past `MAX_TRACKED_POOLS` (100). | Call `remove_tracked_pool` on a stale pool first, or track fewer pools. |
| 8 | `NotTracked` | `remove_tracked_pool` was called with a pool that is not currently tracked. | Check `is_tracked` before removing. |
| 9 | `WindowTooLarge` | `window_seconds` exceeded `MAX_WINDOW_SECONDS` (90 days). | Use a shorter window. |
| 10 | `TooManyPools` | `get_twal_batch` was called with more pools than `MAX_TRACKED_POOLS`. | Split the request into smaller batches. |
| 11 | `CrossContractCallFailed` | A pool's liquidity-oracle call failed at the host level or the callee panicked (non-contract address, buggy pool). Only ever appears as a `TwalEntry.error_code` from `get_twal_all_safe`/`get_twal_batch`, or as the `Err` from `get_twal_all` once any entry fails this way. | Investigate the specific pool with `get_twal_batch([pool], window)`; consider `remove_tracked_pool` if it is permanently dead. |
| 12 | `MissingClAccumulator` | A CL TWAL query found a stored `LiquiditySnapshot` for the pool but no `ClAccumulator` entry, so there is no running liquidity-cumulative to extrapolate to now. | Call `save_cl_snapshot` for the pool (it writes both) before querying `get_cl_twal`. |

`get_twal_all_safe` and `get_twal_batch` never return an `Err` for a single bad
pool — they return a `TwalEntry` with `ok: false` and `error_code` set to one
of the codes above instead, so a single dead pool cannot take down a batch
read. `get_twal_all` is kept on the ABI for existing callers and still
surfaces the first such code as a typed `Err`.

---

## Factory (`contracts/factory`)

Defined in [contracts/factory/src/lib.rs](../contracts/factory/src/lib.rs) as `FactoryError`.

| Code | Symbol | Cause | Remedy |
|------|--------|-------|--------|
| 1 | `AlreadyInitialized` | `initialize` called twice. | Only initialize the factory once after deployment. |
| 2 | `InvalidFeeBps` | Fee outside `[0, 10 000)`. | Correct the fee value to `[0, 9 999]`. |
| 3 | `PoolAlreadyExists` | `create_pool` was called for a `(token_a, token_b)` pair that already has an AMM pool. | Use `get_pool` to retrieve the existing pool address. |
| 4 | `ClPoolAlreadyExists` | `create_cl_pool` was called for a pair/fee that already has a CL pool. | Use `get_cl_pool` to retrieve the existing address. |
| 5 | `ClWasmNotSet` | Tried to create a CL pool before the CL WASM hash was registered via `set_cl_wasm`. | Call `set_cl_wasm` with the uploaded CL contract hash first. |
| 6 | `Unauthorized` | Non-admin called an admin-only factory function. | Use the factory admin keypair. |
| 7 | `FeeNotConfigured` | Attempted pool creation with unconfigured or invalid fee tier. | Use a configured fee tier (e.g., 1, 5, 30, 100 bps). |
| 8 | `RateLimitExceeded` | Pool creation rate limit reached for current epoch. | Wait for rate limit window to reset. |
| 9 | `CreationPaused` | Admin administratively paused pool creation. | Wait for admin to unpause creation. |
| 10 | `NoPendingAdmin` | `accept_admin` was called when no admin transfer is in progress. | Call `propose_admin` first to nominate a successor. |
| 11 | `WrongAdmin` | `accept_admin` was called by an address that does not match the pending nominee. | Have the correct address (the one passed to `propose_admin`) call `accept_admin`. |
| 12 | `NotInitialized` | A function that reads factory configuration (admin, AMM/LP WASM hashes) was called before `initialize`. Covers pool creation (`create_pool`, `create_pool_with_fee_bps`, `create_cl_pool`) and every admin entrypoint. Code 2 is `InvalidFeeBps`, so this code is appended. | Call `initialize` first. |

---

## ClPositionNft (`contracts/cl_position_nft`)

Defined in [contracts/cl_position_nft/src/lib.rs](../contracts/cl_position_nft/src/lib.rs) as `NftError`.

| Code | Symbol | Cause | Remedy |
|------|--------|-------|--------|
| 1 | `AlreadyInitialized` | `initialize` called twice. | Deploy a new NFT contract instead. |
| 2 | `Unauthorized` | A caller other than the registered `cl_pool` called `mint`/`burn`, or `set_ttl_params`/`migrate_ownership_index` was called by a non-admin (or before `initialize`). | Use the registered pool address (for mint/burn) or the admin address (for the others). |
| 3 | `TokenNotFound` | `owner_of`, `position_meta`, `burn`, `approve`, or `transfer` referenced a `token_id` that does not exist (never minted, or already burned). | Verify the id with `owner_of`/`total_supply` first. |
| 4 | `NotOwnerOrApproved` | `transfer` called by an address that is neither the token's owner, its approved address, nor an approved operator for the owner. | Use the owner, an approved address, or an approved operator. |
| 5 | `InvalidReceiver` | Reserved; not currently returned by any function. | — |
| 6 | `InvalidTtlConfig` | `set_ttl_params` was called with `bump_to < min_ttl_threshold`. | Ensure `bump_to >= min_ttl_threshold`. |
| 7 | `TooManyPositions` | `mint` (or the `to` side of `transfer`) would push an owner's O(1)-indexed holdings past `MAX_POSITIONS_PER_OWNER` (10,000). Defence in depth introduced in #697, on top of the O(1) index fix itself. | Transfer or burn existing positions for that owner first. |

#697 replaced the single unbounded `OwnedTokens(owner) -> Vec<u64>` per-owner
list with a constant-cost index (`OwnerTokenCount`, `OwnerTokenByIndex`,
`TokenIndexOfOwner`), so `mint`, `burn`, and `transfer` each touch a fixed
number of storage entries regardless of how many positions an owner holds.
`balance_of`, `tokens_of`, `tokens_of_paginated`, and
`token_of_owner_by_index` read only that index. A legacy `OwnedTokens`
vector from before this upgrade is not written to again — `burn` and the
"from" side of `transfer` fall back to an O(n) removal from it only when a
token has no index slot, and the admin-only `migrate_ownership_index` moves
a legacy vector into the index in bounded chunks. See the "#697: ownership
index" doc comment in `cl_position_nft/src/lib.rs` for the full design and
its trade-offs.

---

## Governance (`contracts/governance`)

Defined in [contracts/governance/src/lib.rs](../contracts/governance/src/lib.rs) as `GovernanceError`.

| Code | Symbol | Cause | Remedy |
|------|--------|-------|--------|
| 1 | `AlreadyInitialized` | Governance initialized twice. | Deploy a new governance contract. |
| 2 | `InvalidVotingPeriod` | Voting period is zero or below the minimum. | Use a period ≥ 1 ledger. |
| 3 | `InvalidTimelock` | `initialize` or `set_timelock_delay` was given a timelock outside `[MIN_TIMELOCK_SECS, MAX_TIMELOCK_SECS]` (`1..=2_592_000` seconds, up to 30 days), or timestamp arithmetic in `propose` / `veto` overflowed (#1105). | Use a timelock in `1..=2_592_000` seconds. |
| 4 | `InvalidQuorumBps` | `initialize` quorum outside `(0, 10 000]`, or `set_quorum_decay_bps_per_day` outside `[0, 10 000]` (#1105). | Use `1..=10_000` for base quorum and `0..=10_000` for daily decay rate. |
| 5 | `InvalidProposerStake` | `initialize` or `set_min_proposer_stake_bps` was given a proposer stake threshold outside `[0, 10 000]` bps (#1105). | Use a value in `0..=10_000` bps. |
| 6 | `InvalidFeeBps` | Fee value out of range. | Use `[0, 10 000)` for swap fees or `[0, 10 000]` for flash loan fees. |
| 7 | `ZeroTotalSupply` | Vote weight computed on a zero LP supply. | Seed the pool with liquidity before creating proposals. |
| 8 | `InsufficientStake` | Proposer's LP stake is below `min_proposer_stake`. | Acquire more LP tokens before proposing. |
| 9 | `ProposalNotFound` | Referenced proposal ID does not exist. | Use `get_proposal` to verify the ID. |
| 10 | `VotingNotStarted` | Tried to vote before the proposal's start block. | Wait until the voting period begins. |
| 11 | `VotingPeriodEnded` | Tried to vote after the voting period closed. | Votes cannot be cast after closure. |
| 12 | `AlreadyExecuted` | `execute` called on a proposal that was already executed. | Each proposal can only be executed once. |
| 13 | `ProposalCancelled` | Action on a cancelled proposal. | The proposal is terminal; create a new one. |
| 14 | `AlreadyVoted` | The caller already cast a vote on this proposal. | Each address can vote once per proposal. |
| 15 | `NoVotingPower` | Caller's LP balance snapshot at proposal creation was zero. | You must hold LP tokens at the proposal creation block to vote. |
| 16 | `VotingPeriodActive` | Tried to execute while voting is still open. | Wait for the voting period to end. |
| 17 | `ProposalExpired` | `execute` called after the execution window expired. | Create a new proposal. |
| 18 | `TimelockNotElapsed` | `execute` called before the timelock delay elapsed. | Wait the full timelock duration after the voting period ends. |
| 19 | `QuorumNotMet` | Total votes did not reach the quorum threshold. | The proposal fails; if needed, create a new one with broader participation. |
| 20 | `ProposalDefeated` | More votes were cast against than for. | Create a new proposal with updated parameters. |
| 21 | `NotProposer` | `cancel` called by someone other than the original proposer. | Only the proposer can cancel before voting ends. |
| 22 | `NoLockedVote` | `unlock_vote` called when no vote was locked. | Only addresses that voted with token lock need to call `unlock_vote`. |
| 23 | `ProposalNotConcluded` | `unlock_vote` or `claim_rewards` called before the proposal concluded. | Wait for the proposal to reach a terminal state. |
| 24 | `CannotDelegateToSelf` | A delegator tried to delegate to themselves. | Delegate to a different address. |
| 25 | `Unauthorized` | Admin-only operation called by non-admin. | Use the governance admin. |
| 26 | `HasDelegated` | Operation requires direct voting power but caller has already delegated. | Undelegate first. |
| 27 | `DelegationCycle` | The delegation would create a cycle (A → B → … → A). | Choose a delegate that is not already part of this principal's delegation chain. |
| 28 | `ProposalVetoed` | A veto multisig vetoed the proposal. | Create a new proposal; adjust to address the veto reason. |
| 29 | `VetoWindowExpired` | Veto attempted after the veto window closed. | Vetoes must be cast within the veto window after voting ends. |
| 30 | `NotVetoMultisig` | Veto called by an address that is not the configured veto multisig. | Only the veto multisig can veto proposals. |
| 31 | `InsufficientSnapshotBal` | Snapshot balance at proposal creation was insufficient. | Acquire more LP tokens before the proposal snapshot block. |
| 32 | `VetoMultisigNotSet` | Veto-related operation called when no veto multisig is configured. | Configure the veto multisig during governance initialization. |
| 33 | `NoPendingAdmin` | `accept_admin` called without a prior `propose_admin`. | Call `propose_admin` first to nominate a successor. |
| 34 | `PartialFactoryUpdate` | `UpdateFactoryGlobalFee` proposal window (`offset`/`limit`) does not cover all factory pools. | Submit proposal covering all registered factory pools. |
| 35 | `NotInitialized` | A function that reads governance configuration (admin, AMM pool, LP token, voting period, timelock, quorum, proposer stake, proposal counter) was called before `initialize`. Covers the admin setters, `propose_admin`, `propose`, `get_params`, and any proposal path that reaches that configuration. Code 2 is `InvalidVotingPeriod`, so this code is appended. | Call `initialize` first. |

---

## IncentiveCampaigns (`contracts/incentive_campaigns`)

Defined in [contracts/incentive_campaigns/src/lib.rs](../contracts/incentive_campaigns/src/lib.rs) as `IncentiveError`.

| Code | Symbol | Cause | Remedy |
|------|--------|-------|--------|
| 1 | `AlreadyInitialized` | `initialize` was called on a contract that already has governance set. | Initialize once upon deployment. |
| 2 | `NotInitialized` | A function was called before `initialize` (governance / id counters unset). | Call `initialize` first. |
| 3 | `Unauthorized` | A governance-only function was called by another address. | Call using the governance address. |
| 4 | `NoPendingGovernance` | `accept_governance` was called with no nomination outstanding. | Have governance call `propose_governance` first. |
| 5 | `NotPendingGovernance` | `accept_governance` was called by an address other than the nominee. | Call from the nominated governance address. |
| 6 | `InvalidCampaignWindow` | `create_campaign` was given `end_time <= start_time`. | Ensure `start_time < end_time`. |
| 7 | `InvalidRewardRate` | `create_campaign` or `set_campaign_rate` was given a zero or negative rate. | Specify a reward rate > 0. |
| 8 | `InvalidFundingAmount` | `create_campaign` was given a zero or negative funding amount. | Supply positive reward funding. |
| 9 | `InsufficientFunding` | Funding is less than `reward_rate * (end_time - start_time)`. | Fund at least the campaign's maximum payout. |
| 10 | `LpTokenMismatch` | The LP token's admin is not the given pool. | Pass the LP token that belongs to the pool. |
| 11 | `CampaignNotFound` | No campaign exists with the given id. | Use an id returned by `create_campaign` / `list_campaigns_paginated`. |
| 12 | `CampaignNotEnded` | `recover_leftover_funds` was called before `end_time`. | Wait for the campaign to end before recovering unallocated funds. |
| 13 | `NoLeftoverFunds` | `recover_leftover_funds` found nothing left to recover. | No action needed; funds fully distributed. |
| 14 | `CampaignInactive` | `claim_rewards` or `recover_leftover_funds` was called on a campaign that has already been deactivated by leftover recovery. | No further claims or recoveries are possible on this campaign. |
| 15 | `CampaignNotStarted` | `claim_rewards` was called before `start_time`. | Wait for the campaign start timestamp. |
| 16 | `NoLpBalance` | The claiming provider holds 0 LP tokens. | Deposit liquidity to earn LP tokens before claiming. |
| 17 | `NoLpSupply` | The LP token's total supply is 0. | Seed the pool with liquidity. |
| 18 | `NoPendingRewards` | The provider has no rewards accrued since their last claim. | Wait for rewards to accumulate over time. |
| 19 | `RecordNotFound` | `get_distribution_record` was given an unknown id. | Use an id from `list_distribution_records` / `get_claim_history`. |



---

## PolVesting (`contracts/pol_vesting`)

Defined in [contracts/pol_vesting/src/lib.rs](../contracts/pol_vesting/src/lib.rs) as `VestingError`.

| Code | Symbol | Cause | Remedy |
|------|--------|-------|--------|
| 1 | `AlreadyInitialized` | Contract already initialized. | Initialize once. |
| 2 | `NotGovernance` | Action called by non-governance address. | Invoke from governance address. |
| 3 | `VestingNotFound` | Vesting schedule does not exist for beneficiary. | Create schedule before releasing/revoking. |
| 4 | `VestingAlreadyExists` | Schedule already exists for beneficiary. | Revoke existing schedule or use new beneficiary. |
| 5 | `NothingToRelease` | Zero vested tokens available to release at current timestamp. | Wait for vesting cliff or schedule progression. |
| 6 | `InvalidSchedule` | `start_time >= end_time` or cliff outside schedule window. | Ensure `start_time < cliff <= end_time`. |
| 7 | `NotBeneficiary` | Release attempted by non-beneficiary account. | Call release from beneficiary address. |
| 8 | `NoPendingGovernance` | `accept_governance` called without prior proposal. | Propose governance transfer first. |
| 9 | `NoPendingTreasury` | `accept_treasury` called without prior proposal. | Propose treasury transfer first. |
| 10 | `NotTreasury` | Action called by non-treasury address. | Call from stored treasury address. |
| 11 | `InsufficientFunding` | `create_vesting` for more tokens than the contract holds outside existing schedules. | Transfer the LP tokens to the contract before creating the schedule. |
| 12 | `TooManySchedules` | Beneficiary already holds the maximum number of live schedules. | Revoke or move a schedule, or vest to a different beneficiary. |

---

## ReserveManager (`contracts/reserve_manager`)

Defined in [contracts/reserve_manager/src/lib.rs](../contracts/reserve_manager/src/lib.rs) as `ReserveManagerError`.

| Code | Symbol | Cause | Remedy |
|------|--------|-------|--------|
| 1 | `NoPendingGovernance` | `accept_governance` called without pending transfer. | Propose governance transfer first. |
| 2 | `Unauthorized` | Action invoked by non-governance account. | Call from authorized governance account. |
| 3 | `AlreadyInitialized` | Reserve manager initialized twice. | Initialize once upon deployment. |
| 4 | `NegativeReserveAmount` | `min_reserve` specified as negative value. | Pass non-negative reserve amount. |
| 5 | `BatchTooLarge` | `check_reserves_batch` called with more than `MAX_PAGE` (50) pools. | Split the pool list into batches of at most 50. |
| 6 | `NoPendingAdmin` | `accept_admin` was called when no admin transfer is in progress. | Call `propose_admin` first to nominate a successor. |
| 7 | `WrongAdmin` | `accept_admin` was called by an address that does not match the pending nominee. | Have the correct address (the one passed to `propose_admin`) call `accept_admin`. |
| 8 | `Paused` | Contract is administratively paused. | Wait for governance to call `unpause`. |

---

## Router (`contracts/router`)

Defined in [contracts/router/src/lib.rs](../contracts/router/src/lib.rs) as `RouterError`.

| Code | Symbol | Cause | Remedy |
|------|--------|-------|--------|
| 1 | `AlreadyInitialized` | `initialize` was called twice (`DataKey::Factory` already exists). | Initialize the router contract once. |
| 2 | `NotInitialized` | A function was called before `initialize` (the factory address is unset). | Call `initialize` first. |
| 3 | `InvalidPath` | `path.len() < 2` in a swap or quote function. | Pass a path containing at least two token addresses. |
| 4 | `DuplicateAdjacentToken` | `path[i] == path[i + 1]`, which resolves to a pool that cannot exist. | Remove the repeated token from the path. |
| 5 | `InvalidAmount` | An input or output amount was zero or negative. | Pass a strictly positive amount. |
| 6 | `DeadlineExceeded` | `env.ledger().timestamp() > deadline`. | Re-submit the swap with a future deadline. |
| 7 | `PoolNotFound` | The factory returned no pool for the token pair at some hop. | Ensure a pool exists for every adjacent pair; call `is_path_routable` to check before quoting. |
| 8 | `SlippageExceeded` | Realized output `< min_amount_out`, or required input `> max_in`. | Widen the slippage bounds or recalculate the path quote. |
| 9 | `Paused` | `swap_exact_in` or `swap_exact_out` was called while the router is paused. Quotes and path views stay callable. | Wait for the admin to call `unpause`; check `is_paused` first. |
| 10 | `Unauthorized` | `pause` or `unpause` was called with an `admin` address that does not match the stored admin. | Pass the stored admin address and sign with its key. |

---

## Staking (`contracts/staking`)

Defined in [contracts/staking/src/lib.rs](../contracts/staking/src/lib.rs) as `StakingError`.

| Code | Symbol | Cause | Remedy |
|------|--------|-------|--------|
| 1 | `AlreadyInitialized` | `initialize` (or `initialize_with_boost_config`) was called on a pool that is already set up. | Initialize the contract once. |
| 2 | `NotInitialized` | A function was called before `initialize` (the LP token / admin is unset). | Call `initialize` first. |
| 3 | `Unauthorized` | A restricted function was called by an address other than the stored admin. | Call from the stored admin address. |
| 4 | `Paused` | `stake`, `stake_locked`, `claim`, or `extend_lock` was attempted while paused. | Wait for the admin to `unpause`. |
| 5 | `InvalidAmount` | An amount argument was zero or negative where a positive value is required. | Provide a strictly positive amount. |
| 6 | `NothingStaked` | An action was attempted by an address with a zero staked balance. | Stake tokens before withdrawing, unlocking, or emergency-withdrawing. |
| 7 | `InsufficientStaked` | An unstake requested more than the staker's staked balance. | Unstake an amount no greater than the staked balance. |
| 8 | `StillLocked` | `unstake` was called before the lock expired (`now < lock_expiry`). | Wait for the lock duration to elapse. |
| 9 | `NoPendingRewards` | `claim` was attempted with zero pending rewards. | Allow rewards to accrue before claiming. |
| 10 | `EmergencyModeNotActive` | `emergency_withdraw` was called while emergency mode is off. | Wait for the admin to enable emergency mode. |
| 11 | `NoActiveLock` | `extend_lock` was called on a position with no active lock. | Stake with a lock duration first. |
| 12 | `InvalidDuration` | A lock duration argument was zero where a positive value is required. | Specify a positive lock duration in seconds. |
| 13 | `NothingToStake` | A stake call supplied neither a positive amount nor a lock duration. | Pass a positive amount, a lock duration, or both. |
| 14 | `InvalidBoostConfig` | The boost bounds were non-positive, or the max was below the min. | Pass `0 < min_boost <= max_boost`. |
| 15 | `InvalidLockDuration` | The lock-duration bounds were non-positive, or the max was below the min. | Pass `0 < min_lock_duration <= max_lock_duration`. |
| 16 | `MaxRewardPoolExceeded` | `add_rewards` would push the undistributed reward balance above the configured cap. | Lower the amount added, or raise the cap with `set_max_reward_pool_balance`. |
| 17 | `InvalidMaxBalance` | A new max-balance cap was set below the current undistributed reward balance. | Use a cap of `0` (no cap) or one at least the current undistributed balance. |
| 18 | `NoStakers` | `update_rewards` was called while the pool has no effective stake. | Wait until at least one address has staked. |
| 19 | `BatchTooLarge` | `settle_boost_batch` or `register_existing_stakers` was given more than `MAX_BATCH_SIZE` (50) addresses. | Split the list into chunks of at most 50 addresses per call. |
| 20 | `NoPendingAdmin` | `accept_admin` was called when no admin transfer is in progress. | Call `propose_admin` first to nominate a successor. |
| 21 | `WrongAdmin` | `accept_admin` was called by an address that does not match the pending nominee. | Have the correct address (the one passed to `propose_admin`) call `accept_admin`. |

---

## Token (`contracts/token`)

Defined in [contracts/token/src/lib.rs](../contracts/token/src/lib.rs) as `TokenError`.

| Code | Symbol | Cause | Remedy |
|------|--------|-------|--------|
| 1 | `AlreadyInitialized` | The token contract was initialized twice. | Initialize once upon deployment. |
| 2 | `NotInitialized` | A function was called before `initialize` (the admin is unset). | Initialize the token first. |
| 3 | `InvalidAmount` | An amount was zero or negative where a positive value is required, or negative where a non-negative value is required. | Pass a valid amount. |
| 4 | `InsufficientBalance` | A transfer, burn, or lock exceeded the account's unlocked balance. | Operate on no more than the available unlocked balance. |
| 5 | `InsufficientAllowance` | `transfer_from` exceeded the spender's approved allowance. | Approve a sufficient allowance before transferring. |
| 6 | `InvalidExpiry` | `approve` was given a `live_until_ledger` before the current ledger for a non-zero amount. | Pass a `live_until_ledger` at or after the current ledger. |
| 7 | `Unauthorized` | `propose_admin` was called by a non-admin, or `accept_admin` by a non-nominee. | Call from the correct admin or nominated pending-admin address. |
| 8 | `InsufficientLocked` | An unlock exceeded the locker's entry for the holder, or the holder's total locked balance. | Unlock no more than the locker locked for the holder. |
| 9 | `MigrationOverflow` | A legacy-lock migration would allocate more than the holder's recorded total locked balance. | Migrate an amount within the total locked balance. |
| 10 | `BalanceUnavailable` | `balance_at` was queried for a ledger whose covering checkpoint has been evicted. | Query a ledger within the retained checkpoint window. |

---

## V2ToV3Migration (`contracts/v2_to_v3_migration`)

Defined in [contracts/v2_to_v3_migration/src/lib.rs](../contracts/v2_to_v3_migration/src/lib.rs) as `MigrationError`.

| Code | Symbol | Cause | Remedy |
|------|--------|-------|--------|
| 1 | `NotInitialized` | Migration contract not initialized. | Call `initialize` first. |
| 2 | `AlreadyInitialized` | `initialize` called twice. | Deploy new migration contract. |
| 3 | `Unauthorized` | Call made by non-admin user. | Use admin credentials. |
| 4 | `ZeroShares` | `migrate_liquidity` called with zero LP shares. | Pass positive share amount. |
| 5 | `InvalidRange` | Tick range `(lower_tick, upper_tick)` invalid. | Ensure `lower_tick < upper_tick` within bounds. |
| 6 | `SlippageExceeded` | V3 liquidity minted fell below slippage limits. | Widen slippage bounds. |
| 7 | `MigrationFailed` | V2 burn or V3 mint contract call failed. | Verify V2 LP balance and V3 parameters. |
| 8 | `TokenMismatch` | V2 pool tokens do not match V3 pool tokens. | Select matching V2 and V3 pools. |
| 9 | `Paused` | `migrate` / `migrate_default` called while the admin has paused the contract. | Wait for the admin to unpause. Previews still work. |
| 10 | `UnknownPool` | A V2 or CL pool is not attested by the factory registry (wrong address, unregistered pool, or a CL pool filed under a different fee tier). | Pass the pool addresses the factory returns from `get_pool` / `get_cl_pool`. No funds move. |
| 11 | `NoPendingAdmin` | `accept_admin` called with no pending nomination. | Ask the admin to call `propose_admin` first. |
| 12 | `WrongAdmin` | `accept_admin` called by an address other than the nominee. | Accept from the nominated address. |
| 13 | `FactoryNotSet` | A factory-validated entry point ran on an instance with no factory. | Admin calls `set_factory`. |
| 14 | `NoDefaultPair` | `migrate_default` / `preview_range_default` called with no default pair stored. | Use `migrate` with explicit pools, or admin calls `set_default_pair`. |
| 15 | `PreviewFailed` | `preview_migration` could not quote the position on the CL pool. | Check the tick range and pool state. |

---

## AmmFuzz (`contracts/amm-fuzz`)

Test-only crate ([contracts/amm-fuzz/src/lib.rs](../contracts/amm-fuzz/src/lib.rs)).

Contains property-based and fuzz testing suites for constant-product invariants and real WASM deployments. Exposes no on-chain contract error codes or panics.

---

## IntegrationTests (`contracts/integration-tests`)

Test harness crate ([contracts/integration-tests/src/lib.rs](../contracts/integration-tests/src/lib.rs)).

Contains integration and upgrade test suites for multi-contract interactions. Exposes no on-chain contract error codes or panics.

---

## TwapConsumer (`contracts/twap_consumer`)

Defined in [contracts/twap_consumer/src/lib.rs](../contracts/twap_consumer/src/lib.rs) as `TwapError`.

| Code | Symbol | Cause | Remedy |
|------|--------|-------|--------|
| 1 | `AlreadyInitialized` | `initialize` was called on a consumer contract that has already been set up. | Do not re-initialize the contract. |
| 2 | `NotInitialized` | Contract function called before `initialize` was performed. | Call `initialize` with keeper address first. |
| 3 | `ZeroWindow` | `window_seconds` argument was zero. | Provide a strictly positive window duration in seconds. |
| 4 | `InsufficientHistory` | Not enough snapshot history exists to cover the requested TWAP window. | Wait for more snapshots to accumulate or reduce the TWAP window size. |
| 5 | `NoSnapshotFound` | Snapshot not found for the specified timestamp or pool key. | Verify the snapshot timestamp exists or wait for the keeper to save a snapshot. |
| 6 | `ElapsedZero` | Elapsed time between snapshots is zero or negative. | Ensure oracle price cumulative timestamps advance. |
| 7 | `InvalidSpotPrice` | Spot price provided for validation is non-positive. | Provide a strictly positive spot price. |
| 8 | `InvalidTwapPrice` | Computed TWAP price is non-positive. | Ensure pool reserves and cumulative prices are positive. |
| 9 | `InvalidDeviationBps` | Deviation threshold is outside `[0, 10 000]` bps. | Provide a deviation threshold between 0 and 10 000 bps. |
| 10 | `NegativeCollateral` | Collateral amount provided is negative. | Provide a non-negative collateral amount. |
| 11 | `PriceManipulated` | Spot price deviates from TWAP beyond allowed threshold. | Reject the trade/valuation or retry with current market prices. |
| 12 | `InvalidRetentionPolicy` | `max_age_seconds` is shorter than the minimum supported TWAP window (`LONGEST_TWAP_WINDOW`). | Set `max_age_seconds` to at least `LONGEST_TWAP_WINDOW` or 0 (disabled). |
| 13 | `Unauthorized` | Non-keeper/admin address attempted an administrative action. | Submit the transaction authenticated by the configured keeper/admin. |
| 14 | `CrossContractCallFailed` | A call into a pool's oracle failed: the pool does not implement the interface its tracked type implies (`get_price_cumulative` for `Amm`, `get_tick_cumulative` for `Cl`), the address is not a contract, or the callee panicked. Returned by the snapshot and TWAP reads, including `get_twap_all`, instead of a host trap (#964). | Check the pool address and type with `get_tracked_pools_typed`. A legacy entry that is really a CL pool is re-typed by calling `save_cl_snapshot` for it. |

---

## Decoding errors from RPC responses

When `stellar-sdk-rs` (or the Soroban RPC) returns a failed invocation, the
result contains an XDR `ScError` with kind `Contract` and a `code` field. Map
the code to the table above using the contract address to identify which enum
applies.

```rust
// Pseudocode — adapt to your stellar-sdk-rs version
match result {
    InvocationResult::Err(ScError::Contract(code)) => {
        match contract_address {
            addr if addr == amm_pool => AmmError::from(code),
            addr if addr == cl_pool  => ClError::from(code),
            addr if addr == factory  => FactoryError::from(code),
            _ => eprintln!("unknown contract error {code}"),
        }
    }
    _ => {}
}
```

All error codes are stable across minor contract upgrades. A code is only
ever removed or renumbered in a major version bump accompanied by a migration
guide in [CHANGELOG.md](../CHANGELOG.md).

---

## Keeping documentation in sync

Error code definitions live alongside the contract source. To run automated verification of documentation sync across all `#[contracterror]` enums:

```bash
make check-docs
```

Or execute the check script directly:

```bash
bash scripts/check_error_docs.sh
```

CI automatically runs `make check-docs` on every pull request and push to prevent documentation drift.
(TODO)

