#!/usr/bin/env bash
# batch_router.sh — end-to-end flow for the batch router: initialize,
# execute_batch over swap / add / remove, atomicity on a failing last op,
# simulate_batch against execution, validate_batch rejections, slippage, and
# the call-savings estimate.
#
# Self-contained: deploys its own factory, token pair, AMM pool and batch
# router, so it runs without deploy.sh:
#   bash scripts/e2e/batch_router.sh
set -Eeuo pipefail

# fresh_deadline — ledger-time deadline 10 minutes from now. Computed at each
# call: the setup deploys take long enough on testnet to pass an earlier one.
fresh_deadline() {
  printf '%s' $(( $(date +%s) + 600 ))
}

# batch_swap POOL TOKEN_IN AMOUNT MIN_OUT DEADLINE — one-op batch JSON.
batch_swap() {
  printf '[{"Swap":{"pool":"%s","token_in":"%s","amount_in":"%s","min_out":"%s","pool_kind":"Amm","zero_for_one":true,"sqrt_price_limit_x96":"0"}}]' \
    "$1" "$2" "$3" "$4"
}

run_batch_router_flow() {
  CURRENT_FLOW="batch_router"

  local trader="$SOURCE_PUBLIC_KEY"
  local factory ta tb pool router
  factory=$(e2e_new_factory)
  ta=$(e2e_new_token BRA)
  tb=$(e2e_new_token BRB)
  pool=$(e2e_new_seeded_pool "$factory" "$ta" "$tb" 1000000000)
  router=$(e2e_deploy batch_router)

  # ── initialize ──────────────────────────────────────────────────────────
  invoke "$router" initialize --factory "$factory" >/dev/null
  expect_fail "batch_router: second initialize is rejected" \
    invoke "$router" initialize --factory "$factory"
  assert_eq "batch_router: max_batch_ops equals the documented ceiling" \
    "$(invoke "$router" max_batch_ops | parse_i128)" "200"

  # ── execute_batch: swap, add liquidity, remove liquidity ────────────────
  local ops results info_before info_after
  info_before=$(invoke "$pool" get_info)
  ops=$(printf '[{"Swap":{"pool":"%s","token_in":"%s","amount_in":"1000000","min_out":"0","pool_kind":"Amm","zero_for_one":true,"sqrt_price_limit_x96":"0"}},{"AddLiquidity":{"pool":"%s","amount_a":"1000000","amount_b":"1000000","min_shares":"0"}},{"RemoveLiquidity":{"pool":"%s","shares":"1000000","min_a":"0","min_b":"0"}}]' \
    "$pool" "$ta" "$pool" "$pool")
  results=$(invoke "$router" execute_batch --caller "$trader" --ops "$ops" --deadline "$(fresh_deadline)")
  assert_eq "batch_router: three results, one per op" "$(count_matches '"Swap"|"AddLiquidity"|"RemoveLiquidity"' <<<"$results")" "3"
  local swap_out
  swap_out=$(printf '%s\n' "$results" | grep -Eo '"Swap":"?[0-9]+' | grep -Eo '[0-9]+' | tail -n 1)
  assert_gt "batch_router: swap returns a positive output" "$swap_out" 0
  info_after=$(invoke "$pool" get_info)
  assert_eq "batch_router: the batch changed the pool's reserve_a" \
    "$(( $(printf '%s\n' "$info_after" | json_num reserve_a) != $(printf '%s\n' "$info_before" | json_num reserve_a) ))" "1"

  # ── atomicity: a failing last op reverts every earlier op ───────────────
  local bal_a_before bal_b_before bal_a_after bal_b_after bad_ops
  bal_a_before=$(balance_of "$ta" "$trader")
  bal_b_before=$(balance_of "$tb" "$trader")
  bad_ops=$(batch_swap "$pool" "$ta" 500000 0 0 | sed 's/^\[//;s/\]$//')
  bad_ops="[${bad_ops},{\"RemoveLiquidity\":{\"pool\":\"$pool\",\"shares\":1000000000000000,\"min_a\":0,\"min_b\":0}}]"
  expect_fail "batch_router: a batch whose last op fails is rejected" \
    invoke "$router" execute_batch --caller "$trader" --ops "$bad_ops" --deadline "$(fresh_deadline)"
  bal_a_after=$(balance_of "$ta" "$trader")
  bal_b_after=$(balance_of "$tb" "$trader")
  assert_eq "batch_router: earlier swap is reverted (token A balance)" "$bal_a_after" "$bal_a_before"
  assert_eq "batch_router: earlier swap is reverted (token B balance)" "$bal_b_after" "$bal_b_before"

  # ── simulate_batch agrees with execution ────────────────────────────────
  local one_swap sim exe
  one_swap=$(batch_swap "$pool" "$ta" 250000 0 0)
  sim=$(invoke "$router" simulate_batch --ops "$one_swap" | grep -Eo '[0-9]+' | tail -n 1)
  exe=$(invoke "$router" execute_batch --caller "$trader" --ops "$one_swap" --deadline "$(fresh_deadline)" \
    | grep -Eo '[0-9]+' | tail -n 1)
  assert_eq "batch_router: simulated output equals executed output" "$sim" "$exe"

  # ── validate_batch rejections ───────────────────────────────────────────
  expect_fail "batch_router: empty batch is rejected (EmptyBatch)" \
    invoke "$router" validate_batch --ops "[]" --deadline "$(fresh_deadline)"
  local too_long
  too_long=$(printf '%s,' $(for _ in $(seq 1 201); do printf '%s ' "$(batch_swap "$pool" "$ta" 1 0 0 | sed 's/^\[//;s/\]$//')"; done) | sed 's/,$//')
  expect_fail "batch_router: a batch above max_batch_ops is rejected (BatchTooLarge)" \
    invoke "$router" validate_batch --ops "[$too_long]" --deadline "$(fresh_deadline)"
  expect_fail "batch_router: an expired deadline is rejected (DeadlineExpired)" \
    invoke "$router" validate_batch --ops "$one_swap" --deadline 1
  local stranger
  stranger=$(e2e_fund_account br-stranger)
  expect_fail "batch_router: an unregistered pool is rejected (PoolNotFound)" \
    invoke "$router" validate_batch --ops "$(batch_swap "$stranger" "$ta" 1 0 0)" --deadline "$(fresh_deadline)"

  # ── slippage ────────────────────────────────────────────────────────────
  expect_fail "batch_router: min_out above the output reverts (SlippageExceeded)" \
    invoke "$router" execute_batch --caller "$trader" \
    --ops "$(batch_swap "$pool" "$ta" 250000 999999999999 0)" --deadline "$(fresh_deadline)"

  # ── call-savings estimate ───────────────────────────────────────────────
  local savings
  savings=$(invoke "$router" estimate_call_savings_v2 --ops_len 3)
  assert_contains "batch_router: estimate reports three individual calls" "$savings" "\"top_level_calls_individual\":3"
  assert_contains "batch_router: estimate reports one batched call" "$savings" "\"top_level_calls_batched\":1"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  # shellcheck source=scripts/e2e/common.sh
  source "$(dirname "${BASH_SOURCE[0]}")/common.sh"
  e2e_standalone run_batch_router_flow
fi
