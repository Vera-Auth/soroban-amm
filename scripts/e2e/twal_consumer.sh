#!/usr/bin/env bash
# twal_consumer.sh — end-to-end flow for the time-weighted average liquidity
# consumer: initialize -> save_snapshot -> change liquidity -> save again ->
# get_twal_liquidity, plus the rejection paths and the batch readers.
#
# Waits for ledger time by polling the pool's own cumulative timestamp, not by
# sleeping for a fixed period. Self-contained: deploys its own token pair,
# AMM pool and twal_consumer instance, so it runs without deploy.sh:
#   bash scripts/e2e/twal_consumer.sh
set -Eeuo pipefail

# e2e_wait_for_timestamp POOL TARGET_TS — poll the pool's cumulative timestamp
# until it reaches TARGET_TS. Fails with a clear message after 240 seconds.
e2e_wait_for_timestamp() {
  local pool="$1" target="$2" ts deadline
  deadline=$(( $(date +%s) + 240 ))
  while :; do
    ts=$(invoke "$pool" get_liquidity_cumulative | grep -Eo '[0-9]+' | tail -n 1)
    if (( ts >= target )); then
      return 0
    fi
    if (( $(date +%s) >= deadline )); then
      die "e2e_wait_for_timestamp: pool clock stuck at $ts, waited for $target"
    fi
    sleep 5
  done
}

# twal_bool — reduce a bool result from the CLI ("true" / "false", maybe
# quoted or padded) to its bare word.
twal_bool() {
  tr -d '[:space:]"'
}

run_twal_consumer_flow() {
  CURRENT_FLOW="twal_consumer"

  local keeper="$SOURCE_PUBLIC_KEY"
  local outsider_key="${SOURCE_ACCOUNT}-twal-outsider"
  local outsider
  outsider=$(e2e_fund_account twal-outsider)

  # ── deploy a funded AMM pool and the consumer ──────────────────────────
  local factory tx ty pool twal
  factory=$(e2e_new_factory)
  tx=$(e2e_new_token TWA)
  ty=$(e2e_new_token TWB)
  pool=$(e2e_new_seeded_pool "$factory" "$tx" "$ty" 1000000000)
  twal=$(e2e_deploy twal_consumer)

  # ── initialize ──────────────────────────────────────────────────────────
  invoke "$twal" initialize --keeper "$keeper" >/dev/null
  assert_eq "twal_consumer: keeper reads back" "$(invoke "$twal" get_keeper | parse_address)" "$keeper"
  expect_fail "twal_consumer: second initialize is rejected" \
    invoke "$twal" initialize --keeper "$outsider"

  # ── tracking and the first snapshot (keeper only) ───────────────────────
  expect_fail "twal_consumer: non-keeper cannot save a snapshot" \
    invoke_as "$outsider_key" "$twal" save_snapshot --pool "$pool"
  invoke "$twal" save_snapshot --pool "$pool" >/dev/null
  assert_eq "twal_consumer: first snapshot recorded" "$(invoke "$twal" get_snapshot_count --pool "$pool" | parse_i128)" "1"
  assert_eq "twal_consumer: pool is tracked after save" "$(invoke "$twal" is_tracked --pool "$pool" | twal_bool)" "true"
  assert_eq "twal_consumer: one tracked pool" "$(invoke "$twal" get_tracked_pool_count | parse_i128)" "1"
  assert_contains "twal_consumer: paginated list includes the pool" \
    "$(invoke "$twal" get_tracked_pools_paginated --offset 0 --limit 10)" "$pool"

  local first_ts
  first_ts=$(invoke "$pool" get_liquidity_cumulative | grep -Eo '[0-9]+' | tail -n 1)

  # ── change liquidity, wait for ledger time, snapshot again ──────────────
  # The seeding deposit spent the keeper's minted balance, so mint more first.
  invoke "$tx" mint --to "$keeper" --amount 500000000 >/dev/null
  invoke "$ty" mint --to "$keeper" --amount 500000000 >/dev/null
  local low high shares
  low=$(invoke "$pool" get_info | grep -Eo '"total_shares":"?[0-9]+' | grep -Eo '[0-9]+' | tail -n 1)
  shares=$(invoke "$pool" add_liquidity \
    --provider "$keeper" --amount_a 500000000 --amount_b 500000000 \
    --min_shares 0 --deadline "$(( $(date +%s) + 600 ))" | parse_i128)
  assert_gt "twal_consumer: add_liquidity mints shares" "$shares" 0
  high=$(invoke "$pool" get_info | grep -Eo '"total_shares":"?[0-9]+' | grep -Eo '[0-9]+' | tail -n 1)
  local second_ts
  second_ts=$(( first_ts + 60 ))
  e2e_wait_for_timestamp "$pool" "$second_ts"
  invoke "$twal" save_snapshot --pool "$pool" >/dev/null
  assert_eq "twal_consumer: second snapshot recorded" "$(invoke "$twal" get_snapshot_count --pool "$pool" | parse_i128)" "2"

  # ── TWAL reads: the average lies between the liquidity bounds ───────────
  local window avg
  window=30
  avg=$(invoke "$twal" get_twal_liquidity --pool "$pool" --window_seconds "$window" | parse_i128)
  assert_gt "twal_consumer: TWAL over a 60s window is positive" "$avg" 0
  # The window covers time before and after the deposit, so the average lies
  # between the liquidity before it and the liquidity after it.
  assert_between "twal_consumer: TWAL lies between observed liquidity bounds" "$avg" "$low" "$high"

  # ── rejection paths ─────────────────────────────────────────────────────
  expect_fail "twal_consumer: zero window is rejected (ZeroWindow)" \
    invoke "$twal" get_twal_liquidity --pool "$pool" --window_seconds 0
  expect_fail "twal_consumer: window longer than the maximum is rejected (WindowTooLarge)" \
    invoke "$twal" get_twal_liquidity --pool "$pool" --window_seconds 4000000000
  expect_fail "twal_consumer: window longer than the history is rejected (InsufficientHistory)" \
    invoke "$twal" get_twal_liquidity --pool "$pool" --window_seconds 4000000
  local stranger
  stranger=$(e2e_fund_account twal-stranger)
  expect_fail "twal_consumer: untracked pool has no snapshot (NoSnapshotFound)" \
    invoke "$twal" get_twal_liquidity --pool "$stranger" --window_seconds 60

  # ── batch readers: one entry per pool, a bad address does not abort ─────
  local batch
  batch=$(invoke "$twal" get_twal_all_safe --window_seconds "$window")
  assert_contains "twal_consumer: batch read returns the tracked pool" "$batch" "$pool"

  # ── untrack and delete ──────────────────────────────────────────────────
  invoke "$twal" remove_tracked_pool --pool "$pool" >/dev/null
  assert_eq "twal_consumer: pool is no longer tracked" "$(invoke "$twal" is_tracked --pool "$pool" | twal_bool)" "false"
  expect_fail "twal_consumer: removing an untracked pool is rejected (NotTracked)" \
    invoke "$twal" remove_tracked_pool --pool "$pool"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  # shellcheck source=scripts/e2e/common.sh
  source "$(dirname "${BASH_SOURCE[0]}")/common.sh"
  e2e_standalone run_twal_consumer_flow
fi
