# Event schema versioning (#302)

Every event emitted by the AMM, CL, governance, factory, token,
reserve_manager (and any future contract that adopts the pattern) carries
a `schema_version: u32`
field. Off-chain consumers (GraphQL indexer, health dashboard, any
WebSocket subscriber) read this field before decoding the rest of
the payload so an unexpected version can be quarantined rather than
silently misinterpreted.

`contracts/twap_consumer/src/lib.rs` emits `snap_del` on snapshot deletion
and `pruned` on bounded pruning sweeps (`(symbol_short!("pruned"), pool)` -> `(removed_count, oldest_remaining_ts)`).


## How it's emitted

Every emit site uses the `soroban_amm_sdk::emit_versioned_event!`
macro instead of `env.events().publish(...)`:

```rust
// Before:
env.events().publish(
    (Symbol::new(&env, "swap"), trader.clone()),
    (token_in, amount_in, token_out, amount_out),
);

// After (#302):
soroban_amm_sdk::emit_versioned_event!(
    env,
    (Symbol::new(&env, "swap"), trader.clone()),
    (token_in, amount_in, token_out, amount_out),
);
```

The macro expands to:

```rust
env.events().publish(
    /* topic */ (Symbol::new( &env, "swap"), trader.clone()),
    /* data  */ (soroban_amm_sdk::EVENT_SCHEMA_VERSION, (token_in, amount_in, token_out, amount_out)),);

```

So the on-wire shape is `(version: u32, original_payload)`. **Topic is
unchanged** -- consumers can keep filtering by event name + author the
way they always have.

## How consumers decode

```rust
let (version, payload): (u32, (Address, i128, Address, i128)) =
    event.data.try_into_val(env)?;

match version {
    1 => decode_v1(payload),
    2 => decode_v2(payload),  // future
    other => {
        log::warn!("unknown event schema version {other}; skipping");
        return Ok(());
    }
}
```

## When to bump `EVENT_SCHEMA_VERSION`

`pub const EVENT_SCHEMA_VERSION: u32 = 1;` lives in
`contracts/amm-sdk/src/lib.rs`.

Bump it (single integer, +1 per release) when ANY event's payload
shape changes:

- Field added
- Field removed
- Field type changed
- Field order changed (Soroban encodes tuples positionally)
- Field renamed (no runtime effect, but indexers that key on names
  will break)

Don't bump for:

- Adding a new event type (consumers can ignore unknown topics)
- Changing event topic content (topic is separate from payload a
  topic change is independently observable)

## Versioning is global, not per-event

One `EVENT_SCHEMA_VERSION` covers every contract event in the
workspace. The alternative -- per-event version -- was rejected because:

1. Consumer state machines would have to track N independent version
   sequences, one per event type.
2. The contracts ship together as a single workspace release; if any
   event payload changes, the deployment cycle re-bumps every consumer
   anyway.
3. A global version is one branch in the consumer's decoder, not N.

The cost is that bumping any event's payload bumps the "version" for
every event, even unchanged ones. That's fine: consumers see the
unchanged payload as the same bytes, just with a different `version`
prefix -- easy to validate during the upgrade window.

## Affected contracts

The catalogue below is the current source-level event inventory for the contracts
that emit versioned events. Keep it synchronized with every
`emit_versioned_event!` call when adding or changing events; do not rely on a
hard-coded site count because tests and helper examples may also contain macro
calls.

`contracts/staking/src/lib.rs` is now fully migrated onto the versioned
scheme (issue #912). Every event it emits goes through
`emit_versioned_event!`; there are no remaining raw `env.events().publish(...)`
calls outside its test modules, and the crate now depends on
`soroban_amm_sdk`. An indexer therefore reads a leading `schema_version`
on every staking event, exactly as it does for the AMM, CL, governance,
and factory contracts.

`contracts/twal_consumer/src/lib.rs` is now migrated (#919): its
`snapshot_deleted` event and the tracked-pool lifecycle events added in #695
(`pool_add`, `pool_remove`) all emit through `emit_versioned_event!`, so the
whole contract carries `EVENT_SCHEMA_VERSION`. See the twal_consumer section in
the catalogue below.

Test files in each of those crates were updated to decode the
versioned payload shape and assert the leading field equals
`EVENT_SCHEMA_VERSION`.

## New events in this change

The factory now emits two additional events:

- `pool_meta`: emitted when pool metadata is created or updated. Payload:
  `(schema_version: u32, (label: String, category: PoolCategory, created_at: u64, created_by: Address, verified: bool))`.
- `pool_ver`: emitted when the verified flag changes. Payload:
  `(schema_version: u32, verified: bool)`.

Consumers should ignore unknown topics; existing versioning rules apply.

## Event catalogue

The complete list of events with their named payload fields, for every
contract, is the generated [Webhook field catalogue](#webhook-field-catalogue-generated)
at the end of this document. It comes from
`services/webhook-streamer/src/event-schemas.json`, which CI checks against the
contracts' emit sites (`scripts/check_event_schemas.mjs`).

Every row below has the on-wire data shape `(schema_version, payload)`.
The topic is the event name, optionally followed by the address shown in the
`topics` column. Field types use the Rust/Soroban names from the emitting
contract.

### Constant-product AMM — `contracts/amm/src/lib.rs`

| Event | Topics | Payload |
|---|---|---|
| `swap` | `trader` | `(token_in: Address, amount_in: i128, token_out: Address, amount_out: i128)` on standard paths; the fee-on-transfer path uses `(token_in, actual_received, token_out, amount_out, referrer: Option<Address>)` |
| `flash_loan` | `receiver` | `(amount_a: i128, amount_b: i128, fee_a: i128, fee_b: i128)` |
| `fot_detected` | `token_in` | `(amount_in: i128, actual_received: i128)` |
| `add_liquidity` | `provider` | `(amount_a: i128, amount_b: i128, shares: i128)` |
| `rm_liq` | — | `(provider: Address, shares: i128, amount_a: i128, amount_b: i128)` |
| `rm_liq_1s` | — | `(provider: Address, shares: i128, token_out: Address, amount_out: i128)` |
| `admin_nominated` | — | `(current_admin: Address, new_admin: Address)` |
| `admin_changed` | — | `(new_admin: Address)` |

| `cb_config` | — | `(threshold_bps: i128, cooldown_secs: u64)` |
| `circuit_break` | — | `(baseline_price: i128, current_price: i128, deviation_bps: i128, threshold_bps: i128)` |
| `emergency_withdraw` | `admin` | `(to: Address, reserve_a: i128, reserve_b: i128)` |
| `flash_fee_upd` | `admin` | `(new_fee_bps: i128)` |
| `lp_rebate_set` | `admin` | `(lp_rebate_bps: i128)` |
| `multisig_set` | `admin` | `(quorum: u32)` |
| `ms_proposed` | `signer` | `(recipient: Address, approvals: u32)` |
| `ms_ew` | `signer` | `(to: Address, reserve_a: i128, reserve_b: i128)` |
| `force_unlock` | — | `(timestamp: u64)` |
| `fee_upd` | `admin` | `(new_fee_bps: i128)` |
| `protocol_fee_set` | — | `(protocol_fee_bps: i128, recipient: Address)` |
| `cb_recovered` | — | `(timestamp: u64)` |
| `upgraded` | — | `(new_wasm_hash: BytesN<32>)` |

The two swap payload variants and all configuration payloads above are emitted
by the current source. Consumers should use the event topic and the global
schema version together when selecting a decoder.

The migration is complete for the constant-product AMM (#921): every emit site
in `contracts/amm/src/lib.rs` now uses `emit_versioned_event!`. The last four
raw `env.events().publish(...)` sites — `fee_upd`, `protocol_fee_set`,
`cb_recovered`, and `upgraded` — were converted, so no unversioned event
remains outside test modules. A regression guard
(`test_every_emitted_event_carries_version_stamp`) walks the full event log for
a representative call sequence and asserts every pool event carries the leading
`EVENT_SCHEMA_VERSION` stamp, rather than checking one topic at a time.

### Concentrated-liquidity AMM — `contracts/concentrated_liquidity/src/lib.rs`

| Event | Topics | Payload |
|---|---|---|
| `mint_pos` | `provider` | `(lower_tick: i32, upper_tick: i32, liquidity: i128, amount_a: i128, amount_b: i128)` |
| `mod_pos` | `provider` | `(lower_tick: i32, upper_tick: i32, liquidity_delta: i128, amount_a: i128, amount_b: i128)` |
| `mint_1t` | `provider` | `(lower_tick: i32, upper_tick: i32, liquidity: i128, amount_used: i128, dust: i128)` |
| `rng_ord` | `provider` | `(lower_tick: i32, upper_tick: i32, liquidity: i128, is_above: bool)` |
| `burn_pos` | `recipient` | `(lower_tick: i32, upper_tick: i32, liquidity: i128, amount_a: i128, amount_b: i128)` |
| `coll_fees` | `recipient` | `(lower_tick: i32, upper_tick: i32, total_a: i128, total_b: i128)` |
| `nft_link` | `provider` | `(token_id: i128, lower_tick: i32, upper_tick: i32)` |
| `swap` | `sender` | `(zero_for_one: bool, amount_in: i128, amount_out: i128, sqrt_price_x96: i128, current_tick: i32, liquidity: i128)` |
| `swap_out` | `sender` | `(zero_for_one: bool, amount_in: i128, amount_out: i128, sqrt_price_x96: i128, current_tick: i32)` |
| `price_upd` | `token_in`, `token_out` | `(amount_in: i128, amount_out: i128, sqrt_price_x96: i128, current_tick: i32)` |

### Factory — `contracts/factory/src/lib.rs`

| Event | Payload |
|---|---|
| `pool_created` | `(token_a: Address, token_b: Address, pool: Address, fee_bps: u32, lp_token: Address, ...)` |
| `cl_pool_created` | `(token_a: Address, token_b: Address, fee_bps: u32, pool: Address)` |
| `wasm_updated` | `(amm_wasm_hash: BytesN<32>, token_wasm_hash: BytesN<32>)` |
| `default_fee_tier_updated` | `(fee_tier: i128)` |
| `upgraded` | `(new_wasm_hash: BytesN<32>)` |
| `creation_paused` / `creation_unpaused` | `(admin: Address)` |
| `mode_changed` | `(enabled: bool)` |
| `creation_fee_set` | `(fee_token: Address, fee_amount: i128)` |
| `treasury_set` | `(treasury: Address, global_protocol_fee_bps: u32, updated: u32)` |
| `global_fee_set` | `(protocol_fee_bps: u32, offset: u32, updated: u32)` |
| `fees_swept` | `(treasury: Address, token: Address, offset: u32, pools_swept: u32, total_collected: i128)` |
| `creation_fee_paid` | `(caller: Address, fee_amount: i128)` |
| `pool_meta` | `(label: String, category: PoolCategory, created_at: u64, created_by: Address, verified: bool)` |
| `pool_ver` | `(verified: bool)` |

### Governance — `contracts/governance/src/lib.rs`

| Event | Payload |
|---|---|
| `admin_nominated` | `(current_admin: Address, new_admin: Address)` |
| `admin_changed` | `(new_admin: Address)` |
| `proposed` | `(id: u64, proposer: Address, kind: ProposalKind, vote_end: u64, snapshot_ledger: u32)` |
| `voted` | `(proposal_id: u64, voter: Address, choice: VoteChoice, voting_power: i128)` |
| `executed` | `(proposal_id: u64, kind: ProposalKind)` |
| `vote_unlocked` | `(proposal_id: u64, locked: i128)` |
| `vetoed` | `(proposal_id: u64, multisig: Address, now: u64, discussion_end: u64)` |

### Token (SEP-41 LP token) — `contracts/token/src/lib.rs`

| Event | Topics | Payload |
|---|---|---|
| `transfer` | `from` | `(to: Address, amount: i128)` |
| `admin_nominated` | — | `(current_admin: Address, new_admin: Address)` |
| `admin_transferred` | — | `(new_admin: Address)` |
| `upgraded` | — | `(new_wasm_hash: BytesN<32>)` |

### Reserve Manager — `contracts/reserve_manager/src/lib.rs`

| Event | Topics | Payload |
|---|---|---|
| `governance_proposed` | — | `(current_governance: Address, new_governance: Address)` |
| `governance_transferred` | — | `(new_governance: Address)` |
| `res_warn` | — | `(unhealthy: Vec<Address>)` |

### POL Vesting — `contracts/pol_vesting/src/lib.rs`

| Event | Topics | Payload |
|---|---|---|
| `governance_proposed` | — | `(current_governance: Address, new_governance: Address)` |
| `governance_transferred` | — | `(old_governance: Address, new_governance: Address)` |
| `treasury_proposed` | — | `(governance: Address, new_treasury: Address)` |
| `treasury_transferred` | — | `(old_treasury: Address, new_treasury: Address)` |
| `vesting_created` | — | `(beneficiary: Address, schedule_id: u32, total: i128, start_ledger: u32, cliff_ledger: u32, end_ledger: u32)` |
| `released` | — | `(beneficiary: Address, schedule_id: u32, amount: i128)` |
| `beneficiary_changed` | — | `(old_beneficiary: Address, old_schedule_id: u32, new_beneficiary: Address, new_schedule_id: u32)` |
| `vesting_revoked` | — | `(beneficiary: Address, schedule_id: u32, to_beneficiary: i128, to_treasury: i128)` |

### Incentive Campaigns — `contracts/incentive_campaigns/src/lib.rs`

| Event | Topics | Payload |
|---|---|---|
| `governance_proposed` | — | `(caller: Address, new_governance: Address)` |
| `governance_transferred` | — | `(old_governance: Address, new_governance: Address)` |
| `campaign_created` | — | `(id: u64, pool: Address, reward_token: Address, start_time: u64, end_time: u64, reward_rate: i128)` |
| `rate_updated` | — | `(campaign_id: u64, new_rate: i128)` |
| `leftover_recovered` | — | `(campaign_id: u64, recipient: Address, leftover: i128)` |
| `reward_distributed` | — | `(campaign_id: u64, provider: Address, amount: i128, dist_id: u64)` |

### TWAL consumer — `contracts/twal_consumer/src/lib.rs`

| Event | Topics | Payload |
|---|---|---|
| `pool_add` | — | `pool: Address` |
| `pool_remove` | — | `pool: Address` |
| `snapshot_deleted` | — | `(pool: Address, ledger_ts: u64)` |

### TWAP consumer — `contracts/twap_consumer/src/lib.rs`

| Event | Topics | Payload |
|---|---|---|
| `snap_del` | `pool` | `ledger_ts: u64` |
| `pruned` | `pool` | `(removed_count: u32, oldest_remaining_ts: u64)` |

### BatchRouter — `contracts/batch_router/src/lib.rs`

| Event | Payload |
|---|---|
| `batch_op` | `(op_index: u32, kind: Symbol, result: BatchOpResult)` — emitted once per operation, in order, before the batch-level event. |
| `batch_executed` | `(ops_len: u32)` |

### Batch Auction — `contracts/batch_auction/src/lib.rs`

| Event | Topics | Payload |
|---|---|---|
| `order_submitted` | `trader` | `(order_id: u64, amount_in: i128)` |
| `order_cancelled` | `trader` | `(order_id: u64)` |
| `order_expired` | `trader` | `(order_id: u64)` |
| `order_failed` | `trader` | `(order_id: u64)` |
| `order_refund_failed` | `trader` | `(order_id: u64)` |
| `order_venue_removed` | `trader` | `(order_id: u64)` |
| `order_settled` | `trader` | `(order_id: u64, amount_out: i128)` |
| `refund_claimed` | `trader` | `(order_id: u64, amount: i128)` |
| `settled` | — | `(process_count: u32)` |
| `window_updated` | — | `(batch_window_secs: u64)` |
| `max_orders_updated` | — | `(max_orders: u32)` |
| `factory_updated` | — | `(factory: Address)` |
| `venue_added` | `pool` | `(pool_type: PoolType)` |
| `venue_removed` | `pool` | `()` |
| `admin_nominated` | — | `(current_admin: Address, new_admin: Address)` |
| `admin_changed` | — | `(new_admin: Address)` |

### V2→V3 Migration — `contracts/v2_to_v3_migration/src/lib.rs`

| Event | Topics | Payload |
|---|---|---|
| `migrated` | `provider` | `(v2_shares: i128, deposited_a: i128, deposited_b: i128, position_token_id: Option<u64>, leftover_a: i128, leftover_b: i128)` |
| `factory_set` | — | `(factory: Address)` |
| `default_pair_set` | — | `(v2_pool: Address, v3_pool: Address)` |
| `paused` | — | `(admin: Address)` |
| `unpaused` | — | `(admin: Address)` |
| `upgraded` | — | `(new_wasm_hash: BytesN<32>)` |
| `admin_nominated` | — | `(admin: Address, new_admin: Address)` |
| `admin_changed` | — | `(new_admin: Address)` |

### BatchRouter — `contracts/batch_router/src/lib.rs`

| Event | Payload |
|---|---|
| `batch_op` | `(op_index: u32, kind: Symbol, result: BatchOpResult)` — emitted once per operation, in order, before the batch-level event. |
| `batch_executed` | `(ops_len: u32)` |

### DEX aggregator — `contracts/dex_aggregator/src/lib.rs`

`venue` is the pool a route is *entered* through: the venue that won the
first-hop decision. `venue_kind` is the `PoolKind` enum (`Amm` for the
constant-product V2 pools, `Cl` for concentrated liquidity).

| Event | Topics | Payload |
|---|---|---|
| `cl_reg` | — | `(token_a: Address, token_b: Address, fee_bps: i128, pool: Address)` |
| `route_sel` | — | `(venue: Address, venue_kind: PoolKind, amount_in: i128, amount_out: i128)` |
| `route_alt` | — | `(venue: Address, amount_out: i128, alt_venue: Address, alt_venue_kind: PoolKind, alt_amount_out: i128)` |
| `route_exe` | — | `(trader: Address, token_in: Address, token_out: Address, amount_in: i128, amount_out: i128, pool: Address)` |
| `tol_fail` | — | `(pool: Address, observed_bps: i128, tolerance_bps: i128)` |
| `pause` / `unpause` | — | `(admin: Address)` |

`route_sel` is emitted by `find_best_route` (and therefore by `get_quote`,
`swap_best` and `is_price_within_tolerance`, which all route through it).
`route_alt` accompanies it only when a second venue actually completed a
quote to the destination token, so the improvement the aggregator delivered
is measurable; a single-venue pair emits `route_sel` alone. `route_exe` is
emitted by `execute_route` (and so by `swap_best`) after the swap settles and
carries the output the pools actually returned, not the quoted amount.
`tol_fail` is emitted only when `is_price_within_tolerance` rejects a quote.

### Router — `contracts/router/src/lib.rs`

| Event | Topics | Payload |
|---|---|---|
| `pause` / `unpause` | — | `(admin: Address)` |

`pause` / `unpause` (#937, #938) are emitted by the admin pause switch on both
the router and the DEX aggregator, after the stored-admin check passes.

### Staking — `contracts/staking/src/lib.rs` (versioned events)

Migrated to `emit_versioned_event!` (issue #912). Every row below has the
on-wire data shape `(schema_version, payload)`.

| Event | Topics | Payload |
|---|---|---|
| `staked` | — | `(staker: Address, amount: i128, boost: i128, lock_expiry: u64)` |
| `unstaked` | — | `(staker: Address, amount: i128, rewards: i128)` |
| `lock_extended` | — | `(staker: Address, boost: i128, lock_expiry: u64)` |
| `claimed` | — | `(staker: Address, rewards: i128)` |
| `rewards_added` | — | `(admin: Address, amount: i128)` |
| `rewards_updated` | — | `(distributable: i128)` |
| `rewards_clamped` | — | `(requested: i128, pool_balance: i128)` |
| `max_reward_pool_balance_set` | — | `(admin: Address, max_balance: i128)` |
| `paused` | — | `(admin: Address)` |
| `unpaused` | — | `(admin: Address)` |
| `emergency_mode` | — | `(admin: Address, enabled: bool)` |
| `emergency_withdraw` | — | `(staker: Address, amount: i128)` |
| `boost_exp` | — | `(staker: Address, previous_boost: i128, settled_boost: i128)` |
| `rewards_reconciled` | — | `(admin: Address, old_undistributed: i128, old_owed: i128, new_undistributed: i128, new_owed: i128)` |

`boost_exp` (issue #699) is emitted by `settle_boost`/`settle_boost_batch`
only when a lock's stored boost is actually reduced from a stale
post-expiry value down to `min_boost` -- it does not fire on a no-op call
(active lock, already-settled staker, or unknown address).

`rewards_reconciled` (issue #1045) is emitted once per completed
`reconcile_reward_accounting` run, with the undistributed/owed counters
before and after the repair.

### Consumer rules

A consumer must inspect the leading `schema_version` before decoding any row in
this catalogue. It may decode a version it explicitly supports, quarantine a
newer version, and ignore an unknown topic. A payload shape change requires a
single global version bump even if the change affects only one event; adding a
new topic does not require a bump by itself.

## Update (#685)

`contracts/dex_aggregator/src/lib.rs` previously emitted nothing at all, so a
swap routed through the aggregator was invisible: the underlying pool emitted
its own `swap` event, but nothing recorded that the aggregator chose that
venue, what the alternatives quoted, or who requested the route. It now emits
five events -- `cl_reg`, `route_sel`, `route_alt`, `route_exe` and `tol_fail`
-- all through `emit_versioned_event!`, listed in the new DEX aggregator table
above and decodable through `soroban_amm_sdk::events`.

The execution event is named `route_exe` rather than `route_exec` because
`symbol_short!` accepts at most nine characters.

## Update (#711)

`contracts/batch_router/src/lib.rs` moved its `batch_executed` event off a
raw `env.events().publish` call and onto `emit_versioned_event!`, and gained
a new per-operation `batch_op` event carrying the op's index, kind, and
result so consumers can attribute effects within a batch without
re-deriving them from `batch_executed` alone. Both rows are listed in the
new BatchRouter event table above.

## Update (#698)

`contracts/amm/src/lib.rs` gained one more versioned emit site: the new
admin/multisig-gated `force_unlock` recovery function emits a `force_unlock`
event through `emit_versioned_event!`, consistent with every other
state-mutating call in this contract. The row for it is listed in the
Constant-product AMM event table above.

## Update (#696)

`contracts/concentrated_liquidity/src/lib.rs` gained one more versioned
emit site: `swap_exact_out` emits a `swap_out` event through
`emit_versioned_event!` (payload shape mirrors the existing `swap` event),
plus reuses the existing `price_upd` event on a price-moving trade,
consistent with every other state-mutating call in this contract. The row
for it is listed in the Concentrated-liquidity AMM event table above.

## Update (#689)

`contracts/oracle_aggregator/src/lib.rs` gained a new `src_wt` event emitted
by `set_source_weight`. It has since been migrated onto the versioned scheme
(see #916 below).

### Oracle Aggregator — `contracts/oracle_aggregator/src/lib.rs`

| Event | Topics | Payload |
|---|---|---|
| `stale_src` | — | `(stale_sources: Vec<Address>)` |
| `deviant` | — | `(deviant_sources: Vec<Address>)` |
| `price` | — | `(token_a: Address, token_b: Address, price: i128, confidence: u32)` |
| `src_wt` | — | `(source: Address, old_weight: u32, new_weight: u32)` |

## Update (#812)

`contracts/batch_router/src/lib.rs`, `contracts/batch_auction/src/lib.rs`,
and `contracts/v2_to_v3_migration/src/lib.rs` have all migrated from raw
`env.events().publish()` calls to `emit_versioned_event!`. All event rows for
these three contracts are now listed in the event catalogue above under the
BatchRouter, Batch Auction, and V2→V3 Migration sections respectively.

## Update (#917 #918 #922 #923)

The event-schema migration is now complete for four more contracts. No raw
`env.events().publish(...)` call remains in any of their `src/` trees outside
test modules.

- `contracts/batch_auction/src/lib.rs` (#922) was half-migrated: the remaining
  raw publish sites (`order_venue_removed`, `order_refund_failed`,
  `order_expired`, `refund_claimed`, `factory_updated`, `venue_added`,
  `venue_removed`) now emit through `emit_versioned_event!`. A single guard
  test walks the whole event log for a representative call sequence and asserts
  every payload this contract emits begins with `EVENT_SCHEMA_VERSION`, so the
  half-migrated state cannot recur.
- `contracts/factory/src/lib.rs` (#923) was half-migrated: the remaining raw
  publish sites (`default_fee_tier_updated`, `upgraded`) now emit through
  `emit_versioned_event!`, guarded by the same whole-log walk.
- `contracts/token/src/lib.rs` (#917), the SEP-41 LP token, emitted its entire
  event surface (`transfer`, `admin_nominated`, `admin_transferred`,
  `upgraded`) through raw publish calls and is now fully versioned, with one
  test per topic decoding the payload as a `(u32, T)` pair.
- `contracts/reserve_manager/src/lib.rs` (#918) emitted its entire event
  surface (`governance_proposed`, `governance_transferred`, `res_warn`) through
  raw publish calls and is now fully versioned, with a per-topic test for each.

All event rows for these contracts are listed in the catalogue above under the
Factory, Token, Reserve Manager, and Batch Auction sections.

## Update (#913 #914 #919 #920)

Four more contracts that emitted their entire event surface through raw
`env.events().publish(...)` calls are now fully versioned. No raw publish call
remains in any of their `src/` trees outside test modules, and each has one
test per event topic decoding the payload as a `(u32, T)` pair and asserting the
leading field equals `EVENT_SCHEMA_VERSION`.

- `contracts/pol_vesting/src/lib.rs` (#913): eight events covering the
  governance/treasury handover, schedule lifecycle, releases, and revocations.
- `contracts/incentive_campaigns/src/lib.rs` (#914): six events covering
  governance handover, campaign creation, rate updates, leftover recovery, and
  reward distribution.
- `contracts/twal_consumer/src/lib.rs` (#919): `pool_add`, `pool_remove`, and
  `snapshot_deleted`.
- `contracts/twap_consumer/src/lib.rs` (#920): `snap_del` and `pruned`.

All event rows for these contracts are listed in the catalogue above under the
POL Vesting, Incentive Campaigns, TWAL consumer, and TWAP consumer sections.

## Update (#916)

`contracts/oracle_aggregator/src/lib.rs` is now fully migrated onto the
versioned scheme. All four raw `env.events().publish(...)` sites
(`price`, `src_wt`, `stale_src`, `deviant`) now emit through
`emit_versioned_event!`, with one test per topic decoding the payload as a
version-stamped `(u32, T)` pair in the style of `last_versioned_event` from
the governance test module. No raw `env.events().publish` call remains in
`contracts/oracle_aggregator/src/` outside test modules. The event row for
this contract, listed above under Oracle Aggregator, is unchanged except for
the leading `schema_version` field now present on the wire.

## Update (#915)

`contracts/cl_position_nft/src/lib.rs` is now fully migrated onto the
versioned scheme. All five raw `env.events().publish(...)` sites
(`nft_mint`, `nft_burn`, `approve`, `approval_for_all`, `transfer`) now emit
through `emit_versioned_event!`, with one test per topic decoding the payload
as a version-stamped `(u32, T)` pair. No raw `env.events().publish` call
remains in `contracts/cl_position_nft/src/` outside test modules.

### CL Position NFT — `contracts/cl_position_nft/src/lib.rs`

| Event | Topics | Payload |
|---|---|---|
| `nft_mint` | `to` | `(token_id: u64,)` |
| `nft_burn` | `owner` | `(token_id: u64,)` |
| `approve` | `caller`, `approved` | `(token_id: u64,)` |
| `approval_for_all` | `owner`, `operator` | `(approved: bool,)` |
| `transfer` | `from`, `to` | `(token_id: u64,)` |

<!-- BEGIN GENERATED EVENT FIELD CATALOGUE -->
<!-- Generated by scripts/check_event_schemas.mjs --write-docs from
     services/webhook-streamer/src/event-schemas.json. Do not edit by hand. -->

## Webhook field catalogue (generated)

Every event the deployed contracts emit, with the named payload fields the
webhook-streamer delivers. `Topics` counts the on-chain topics (the first is the
event name; any others, such as the trader or provider, are indexed addresses).
An event listed more than once has several payload shapes, told apart by field
count. Events marked unversioned carry their payload without the
`(EVENT_SCHEMA_VERSION, payload)` envelope.

### `amm`

| Event | Topics | Payload fields | Envelope |
|---|---|---|---|
| `lp_locker_set` | 1 | `lp_token`, `locker` | v1 |
| `emergency_withdraw` | 2 | `to`, `reserve_a`, `reserve_b` | v1 |
| `force_unlock` | 1 | `timestamp` | v1 |
| `cb_config` | 1 | `threshold_bps`, `cooldown_secs` | v1 |
| `cb_recovered` | 1 | `timestamp` | v1 |
| `circuit_break` | 1 | `price_before`, `price_after`, `deviation_bps`, `threshold_bps` | v1 |
| `protocol_fee_set` | 1 | `protocol_fee_bps`, `recipient` | v1 |
| `lp_rebate_set` | 2 | `lp_rebate_bps` | v1 |
| `multisig_set` | 2 | `quorum` | v1 |
| `ms_proposed` | 2 | `recipient`, `approvals` | v1 |
| `ms_ew` | 2 | `to`, `reserve_a`, `reserve_b` | v1 |
| `fee_upd` | 2 | `new_fee_bps` | v1 |
| `flash_fee_upd` | 2 | `new_fee_bps` | v1 |
| `admin_nominated` | 1 | `current_admin`, `new_admin` | v1 |
| `admin_changed` | 1 | `new_admin` | v1 |
| `upgraded` | 1 | `new_wasm_hash` | v1 |
| `add_liquidity` | 2 | `amount_a`, `amount_b`, `shares_minted` | v1 |
| `rm_liq` | 1 | `provider`, `shares_burned`, `amount_a`, `amount_b` | v1 |
| `rm_liq_1s` | 1 | `provider`, `shares_burned`, `token_out`, `total_out` | v1 |
| `swap` | 2 | `token_in`, `amount_in`, `token_out`, `amount_out` | v1 |
| `swap` | 2 | `token_in`, `actual_received`, `token_out`, `amount_out`, `referrer` | v1 |
| `flash_loan` | 2 | `amount_a`, `amount_b`, `fee_a`, `fee_b` | v1 |
| `fot_detected` | 2 | `amount_in`, `actual_received` | v1 |

### `batch_auction`

| Event | Topics | Payload fields | Envelope |
|---|---|---|---|
| `order_submitted` | 2 | `id`, `amount_in` | v1 |
| `order_cancelled` | 2 | `order_id` | v1 |
| `order_expired` | 2 | `order_id` | v1 |
| `order_venue_removed` | 2 | `order_id` | v1 |
| `order_settled` | 2 | `order_id`, `amount_out` | v1 |
| `order_failed` | 2 | `order_id` | v1 |
| `settled` | 1 | `process_count` | v1 |
| `order_refund_failed` | 2 | `order_id` | v1 |
| `refund_claimed` | 2 | `order_id`, `amount` | v1 |
| `window_updated` | 1 | `batch_window_secs` | v1 |
| `max_orders_updated` | 1 | `n` | v1 |
| `factory_updated` | 1 | `factory` | v1 |
| `venue_added` | 2 | `pool_type` | v1 |
| `venue_removed` | 2 | _none_ | v1 |
| `admin_nominated` | 1 | `current_admin`, `new_admin` | v1 |
| `admin_changed` | 1 | `new_admin` | v1 |

### `batch_router`

| Event | Topics | Payload fields | Envelope |
|---|---|---|---|
| `batch_op` | 2 | `index`, `op_kind`, `result` | v1 |
| `batch_executed` | 2 | `op_count` | v1 |

### `cl_position_nft`

| Event | Topics | Payload fields | Envelope |
|---|---|---|---|
| `nft_mint` | 2 | `token_id` | v1 |
| `nft_burn` | 2 | `token_id` | v1 |
| `approve` | 3 | `token_id` | v1 |
| `approval_for_all` | 3 | `approved` | v1 |
| `transfer` | 3 | `token_id` | v1 |

### `concentrated_liquidity`

| Event | Topics | Payload fields | Envelope |
|---|---|---|---|
| `upgraded` | 1 | `new_wasm_hash` | v1 |
| `mint_pos` | 2 | `lower_tick`, `upper_tick`, `liquidity`, `amount_a`, `amount_b` | v1 |
| `mod_pos` | 2 | `lower_tick`, `upper_tick`, `liquidity_delta`, `amount_a`, `amount_b` | v1 |
| `mint_1t` | 2 | `lower_tick`, `upper_tick`, `liquidity`, `amount_used`, `dust` | v1 |
| `rng_ord` | 2 | `lower_tick`, `upper_tick`, `liquidity`, `is_above` | v1 |
| `burn_pos` | 2 | `lower_tick`, `upper_tick`, `liquidity`, `amount_a`, `amount_b` | v1 |
| `coll_fees` | 2 | `lower_tick`, `upper_tick`, `amount_a`, `amount_b` | v1 |
| `nft_link` | 2 | `token_id`, `lower_tick`, `upper_tick` | v1 |
| `swap` | 2 | `zero_for_one`, `amount_in`, `amount_out`, `sqrt_price_x96`, `tick` | v1 |
| `price_upd` | 3 | `amount_in`, `amount_out`, `sqrt_price_x96`, `tick` | v1 |
| `swap_out` | 2 | `zero_for_one`, `amount_in`, `amount_out`, `sqrt_price_x96`, `tick` | v1 |
| `oracle_set` | 1 | `old_oracle`, `new_oracle` | v1 |
| `position_nft_set` | 1 | `old_position_nft`, `new_position_nft` | v1 |
| `max_oracle_deviation_set` | 1 | `old_deviation_bps`, `new_deviation_bps` | v1 |
| `pause` | 1 | `admin` | v1 |
| `unpause` | 1 | `admin` | v1 |
| `admin_nominated` | 1 | `admin`, `new_admin` | v1 |
| `admin_changed` | 1 | `new_admin` | v1 |
| `protocol_fee_set` | 1 | `bps`, `recipient` | v1 |
| `protocol_fees_withdrawn` | 1 | `recipient`, `accrued_a`, `accrued_b` | v1 |

### `dex_aggregator`

| Event | Topics | Payload fields | Envelope |
|---|---|---|---|
| `pause` | 1 | `admin` | v1 |
| `unpause` | 1 | `admin` | v1 |
| `cl_reg` | 1 | `token_a`, `token_b`, `fee_bps`, `pool` | v1 |
| `route_sel` | 1 | `venue`, `venue_kind`, `amount_in`, `amount_out` | v1 |
| `route_alt` | 1 | `venue`, `amount_out`, `alt_venue`, `alt_venue_kind`, `alt_amount_out` | v1 |
| `route_exe` | 1 | `trader`, `token_in`, `token_out`, `amount_in`, `amount_out`, `pool` | v1 |
| `tol_fail` | 1 | `pool`, `observed_bps`, `tolerance_bps` | v1 |

### `factory`

| Event | Topics | Payload fields | Envelope |
|---|---|---|---|
| `pool_created` | 1 | `ta`, `tb`, `pool_addr`, `fee_bps`, `lp_addr` | v1 |
| `wasm_updated` | 1 | `amm_wasm_hash`, `token_wasm_hash` | v1 |
| `default_fee_tier_updated` | 1 | `fee_tier` | v1 |
| `upgraded` | 1 | `new_wasm_hash` | v1 |
| `creation_paused` | 1 | `admin` | v1 |
| `creation_unpaused` | 1 | `admin` | v1 |
| `cl_pool_created` | 1 | `ta`, `tb`, `fee_bps`, `pool_addr` | v1 |
| `mode_changed` | 1 | `enabled` | v1 |
| `creation_fee_set` | 1 | `fee_token`, `fee_amount` | v1 |
| `admin_nominated` | 1 | `admin`, `new_admin` | v1 |
| `admin_changed` | 1 | `new_admin` | v1 |
| `treasury_set` | 1 | `treasury`, `global_protocol_fee_bps`, `updated` | v1 |
| `global_fee_set` | 1 | `protocol_fee_bps`, `offset`, `updated` | v1 |
| `fees_swept` | 1 | `treasury`, `token`, `offset`, `pools_swept`, `total_collected` | v1 |
| `creation_fee_paid` | 1 | `caller`, `fee_amount` | v1 |
| `initialized` | 1 | `admin`, `amm_wasm_hash`, `token_wasm_hash` | v1 |
| `cl_wasm_updated` | 1 | `old_wasm_hash`, `new_wasm_hash` | v1 |
| `rate_limit_set` | 1 | `old_limit`, `new_limit` | v1 |

### `governance`

| Event | Topics | Payload fields | Envelope |
|---|---|---|---|
| `initialized` | 1 | `admin`, `amm_pool`, `lp_token`, `voting_period_secs`, `timelock_secs`, `quorum_bps`, `min_proposer_stake_bps` | v1 |
| `admin_nominated` | 1 | `current_admin`, `new_admin` | v1 |
| `admin_changed` | 1 | `new_admin` | v1 |
| `upgraded` | 1 | `new_wasm_hash` | v1 |
| `quorum_decay_set` | 1 | `old_rate_bps_per_day`, `new_rate_bps_per_day` | v1 |
| `min_stake_set` | 1 | `old_min_proposer_stake_bps`, `new_min_proposer_stake_bps` | v1 |
| `timelock_set` | 1 | `old_timelock_secs`, `new_timelock_secs` | v1 |
| `proposed` | 1 | `id`, `proposer`, `kind`, `vote_end`, `snapshot_ledger` | v1 |
| `voted` | 1 | `proposal_id`, `voter`, `choice`, `voting_power` | v1 |
| `executed` | 1 | `proposal_id`, `kind` | v1 |
| `cancelled` | 1 | `proposal_id` | v1 |
| `vote_unlocked` | 2 | `proposal_id`, `locked` | v1 |
| `delegated` | 1 | `from`, `to` | v1 |
| `undelegated` | 1 | `from` | v1 |
| `veto_multisig_set` | 1 | `old_multisig`, `new_multisig` | v1 |
| `vetoed` | 1 | `proposal_id`, `multisig`, `now`, `discussion_end` | v1 |

### `incentive_campaigns`

| Event | Topics | Payload fields | Envelope |
|---|---|---|---|
| `governance_proposed` | 1 | `caller`, `new_governance` | v1 |
| `governance_transferred` | 1 | `old_governance`, `new_governance` | v1 |
| `campaign_created` | 1 | `id`, `pool`, `reward_token`, `start_time`, `end_time`, `reward_rate` | v1 |
| `rate_updated` | 1 | `campaign_id`, `new_rate` | v1 |
| `leftover_recovered` | 1 | `campaign_id`, `recipient`, `leftover` | v1 |
| `reward_distributed` | 1 | `campaign_id`, `provider`, `pending`, `dist_id` | v1 |

### `oracle_aggregator`

| Event | Topics | Payload fields | Envelope |
|---|---|---|---|
| `admin_nominated` | 1 | `current_admin`, `new_admin` | v1 |
| `admin_changed` | 1 | `new_admin` | v1 |
| `upgraded` | 1 | `new_wasm_hash` | v1 |
| `pause` | 1 | _none_ | unversioned |
| `unpause` | 1 | _none_ | unversioned |
| `price` | 1 | `token_a`, `token_b`, `price`, `confidence` | v1 |
| `src_wt` | 1 | `source_contract`, `old_weight`, `weight` | v1 |
| `stale_src` | 1 | `stale_sources` | v1 |
| `deviant` | 1 | `deviant_sources` | v1 |

### `pol_vesting`

| Event | Topics | Payload fields | Envelope |
|---|---|---|---|
| `governance_proposed` | 1 | `current_governance`, `new_governance` | v1 |
| `governance_transferred` | 1 | `old_governance`, `new_governance` | v1 |
| `treasury_proposed` | 1 | `governance`, `new_treasury` | v1 |
| `treasury_transferred` | 1 | `old_treasury`, `new_treasury` | v1 |
| `vesting_created` | 1 | `beneficiary`, `schedule_id`, `total`, `start_ledger`, `cliff_ledger`, `end_ledger` | v1 |
| `released` | 1 | `beneficiary`, `schedule_id`, `releasable` | v1 |
| `beneficiary_changed` | 1 | `old_beneficiary`, `old_schedule_id`, `new_beneficiary`, `new_schedule_id` | v1 |
| `vesting_revoked` | 1 | `beneficiary`, `schedule_id`, `to_beneficiary`, `to_treasury` | v1 |

### `reserve_manager`

| Event | Topics | Payload fields | Envelope |
|---|---|---|---|
| `governance_proposed` | 1 | `current_governance`, `new_governance` | v1 |
| `governance_transferred` | 1 | `new_governance` | v1 |
| `pause` | 1 | _none_ | v1 |
| `unpause` | 1 | _none_ | v1 |
| `admin_nominated` | 1 | `admin`, `new_admin` | v1 |
| `admin_changed` | 1 | `new_admin` | v1 |
| `res_warn` | 1 | `unhealthy` | v1 |

### `router`

| Event | Topics | Payload fields | Envelope |
|---|---|---|---|
| `pause` | 1 | `admin` | v1 |
| `unpause` | 1 | `admin` | v1 |

### `staking`

| Event | Topics | Payload fields | Envelope |
|---|---|---|---|
| `lock_extended` | 1 | `staker`, `boost`, `expiry` | v1 |
| `rewards_added` | 1 | `admin`, `received` | v1 |
| `paused` | 1 | `admin` | v1 |
| `unpaused` | 1 | `admin` | v1 |
| `admin_nominated` | 1 | `admin`, `new_admin` | unversioned |
| `admin_changed` | 1 | `new_admin` | unversioned |
| `upgraded` | 1 | `new_wasm_hash` | unversioned |
| `staked` | 1 | `staker`, `amount`, `new_boost`, `new_expiry` | v1 |
| `unstaked` | 1 | `staker`, `amount`, `rewards` | v1 |
| `emergency_mode` | 1 | `admin`, `enabled` | v1 |
| `max_reward_pool_balance_set` | 1 | `admin`, `max_balance` | v1 |
| `emergency_withdraw` | 1 | `staker`, `staked_amount` | v1 |
| `boost_exp` | 1 | `staker`, `stored_boost`, `min_boost` | v1 |
| `rewards_reconciled` | 1 | `admin`, `old_undistributed`, `old_owed`, `undistributed`, `owed` | v1 |
| `rewards_clamped` | 1 | `new_rewards`, `pool_balance` | v1 |
| `rewards_updated` | 1 | `distributable` | v1 |
| `claimed` | 1 | `staker`, `pending` | v1 |

### `token`

| Event | Topics | Payload fields | Envelope |
|---|---|---|---|
| `admin_nominated` | 1 | `current_admin`, `new_admin` | v1 |
| `admin_transferred` | 1 | `new_admin` | v1 |
| `upgraded` | 1 | `new_wasm_hash` | v1 |
| `transfer` | 3 | `amount` | unversioned |
| `initialized` | 1 | `admin`, `name`, `symbol`, `decimals` | v1 |
| `approve` | 3 | `amount`, `live_until_ledger` | unversioned |
| `mint` | 3 | `amount` | unversioned |
| `burn` | 2 | `amount` | unversioned |
| `set_locker` | 1 | `admin`, `locker` | v1 |
| `lock` | 1 | `holder`, `locker`, `amount`, `new_entry`, `new_locked` | v1 |
| `unlock` | 1 | `holder`, `locker`, `amount`, `new_entry`, `new_locked` | v1 |
| `migrate_legacy_lock` | 1 | `holder`, `locker`, `amount`, `new_entry`, `grand_total` | v1 |

### `twal_consumer`

| Event | Topics | Payload fields | Envelope |
|---|---|---|---|
| `pool_add` | 1 | `pool` (bare value) | v1 |
| `pool_remove` | 1 | `pool` (bare value) | v1 |
| `snapshot_deleted` | 1 | `pool`, `ledger_ts` | v1 |

### `twap_consumer`

| Event | Topics | Payload fields | Envelope |
|---|---|---|---|
| `snap_del` | 2 | `ledger_ts` (bare value) | v1 |
| `pruned` | 2 | `removed_count`, `oldest_remaining_ts` | v1 |

### `v2_to_v3_migration`

| Event | Topics | Payload fields | Envelope |
|---|---|---|---|
| `migrated` | 2 | `v2_shares`, `deposited_a`, `deposited_b`, `position_token_id`, `leftover_a`, `leftover_b` | v1 |
| `factory_set` | 1 | `factory` | v1 |
| `default_pair_set` | 1 | `v2_pool`, `v3_pool` | v1 |
| `paused` | 1 | `admin` | v1 |
| `unpaused` | 1 | `admin` | v1 |
| `upgraded` | 1 | `new_wasm_hash` | v1 |
| `admin_nominated` | 1 | `admin`, `new_admin` | v1 |
| `admin_changed` | 1 | `new_admin` | v1 |

<!-- END GENERATED EVENT FIELD CATALOGUE -->
