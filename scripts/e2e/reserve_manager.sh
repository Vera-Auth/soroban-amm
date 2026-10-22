#!/usr/bin/env bash
# reserve_manager.sh — end-to-end flow for the reserve manager: initialize,
# per-pair reserve floors, AMM and CL health checks, the batch check, pause,
# and the two-step admin handover.
#
# Self-contained: deploys its own factory, token pairs, AMM pool, CL pool and
# reserve_manager, so it runs without deploy.sh:
#   bash scripts/e2e/reserve_manager.sh
set -Eeuo pipefail

run_reserve_manager_flow() {
  CURRENT_FLOW="reserve_manager"

  local governance="$SOURCE_PUBLIC_KEY"
  local outsider_key="${SOURCE_ACCOUNT}-rm-outsider"
  local outsider nominee
  outsider=$(e2e_fund_account rm-outsider)
  nominee=$(e2e_fund_account rm-nominee)

  # ── deploy the pools and the manager ────────────────────────────────────
  local factory ta tb amm tc td cl rm
  factory=$(e2e_new_factory)
  ta=$(e2e_new_token RMA)
  tb=$(e2e_new_token RMB)
  amm=$(e2e_new_seeded_pool "$factory" "$ta" "$tb" 1000000000)
  tc=$(e2e_new_token RMC)
  td=$(e2e_new_token RMD)
  # The factory needs the CL WASM hash before it can create CL pools.
  # deploy.sh already uploaded the CL WASM and saved its hash; reuse it.
  invoke "$factory" set_cl_wasm_hash --cl_wasm_hash "${CL_WASM_HASH:-$(e2e_upload concentrated_liquidity)}" >/dev/null
  cl=$(invoke "$factory" create_cl_pool \
    --caller "$governance" --token_a "$tc" --token_b "$td" \
    --fee_bps 30 --initial_tick 0 | extract_contract_id)
  rm=$(e2e_deploy reserve_manager)

  # ── initialize ──────────────────────────────────────────────────────────
  invoke "$rm" initialize --governance "$governance" --factory "$factory" >/dev/null
  assert_eq "reserve_manager: governance reads back" "$(invoke "$rm" get_governance | parse_address)" "$governance"
  assert_eq "reserve_manager: factory reads back" "$(invoke "$rm" get_factory | parse_address)" "$factory"
  expect_fail "reserve_manager: second initialize is rejected" \
    invoke "$rm" initialize --governance "$outsider" --factory "$outsider"

  # ── reserve floors for the AMM pair ─────────────────────────────────────
  invoke "$rm" set_min_reserve --token_a "$ta" --token_b "$tb" \
    --min_reserve_a 100000 --min_reserve_b 200000 >/dev/null
  local forward reverse
  forward=$(invoke "$rm" get_min_reserve --token_a "$ta" --token_b "$tb")
  reverse=$(invoke "$rm" get_min_reserve --token_a "$tb" --token_b "$ta")
  # The pair is stored in canonical order, so the per-token floors come back
  # relative to that order. Reading in the reverse order must return the same
  # two floors, swapped.
  local fwd_a fwd_b rev_a rev_b
  fwd_a=$(printf '%s\n' "$forward" | json_num min_reserve_a)
  fwd_b=$(printf '%s\n' "$forward" | json_num min_reserve_b)
  rev_a=$(printf '%s\n' "$reverse" | json_num min_reserve_a)
  assert_eq "reserve_manager: the two stored floors are the ones set" "$(( fwd_a + fwd_b ))" "300000"
  assert_eq "reserve_manager: reverse-order read swaps the floors" "$rev_a" "$fwd_b"
  assert_eq "reserve_manager: one configured pair" "$(invoke "$rm" get_configured_pair_count | parse_i128)" "1"
  assert_contains "reserve_manager: pair is listed" \
    "$(invoke "$rm" list_configured_pairs --offset 0 --limit 10)" "$ta"
  expect_fail "reserve_manager: negative floor is rejected" \
    invoke "$rm" set_min_reserve --token_a "$ta" --token_b "$tb" --min_reserve_a -1 --min_reserve_b 0

  # ── AMM health: healthy above the floor, unhealthy after a withdrawal ───
  assert_eq "reserve_manager: AMM pool is healthy above the floor" \
    "$(invoke "$rm" check_reserves --pool "$amm" | tr -d '[:space:]"')" "true"

  local info total shares out_a reserve_a_after min_a shortfall
  info=$(invoke "$amm" get_info)
  total=$(printf '%s\n' "$info" | json_num total_shares)
  # The seeding deposit minted all shares to the source, minus the locked minimum.
  shares=$(( total / 2 ))
  invoke "$amm" remove_liquidity --provider "$governance" --shares "$shares" \
    --min_a 0 --min_b 0 --deadline "$(( $(date +%s) + 600 ))" >/dev/null
  invoke "$rm" set_min_reserve --token_a "$ta" --token_b "$tb" \
    --min_reserve_a 900000000 --min_reserve_b 0 >/dev/null
  local detailed
  detailed=$(invoke "$rm" check_reserves_detailed --pool "$amm")
  reserve_a_after=$(printf '%s\n' "$detailed" | json_num reserve_a)
  min_a=$(printf '%s\n' "$detailed" | json_num min_a)
  shortfall=$(( min_a - reserve_a_after ))
  assert_eq "reserve_manager: AMM pool is unhealthy after the withdrawal" \
    "$(printf '%s\n' "$detailed" | grep -Eo '"healthy":(true|false)' | tail -n 1 | cut -d: -f2)" "false"
  assert_gt "reserve_manager: the withdrawal left a shortfall" "$shortfall" 0
  assert_eq "reserve_manager: the shortfall is the exact gap to the floor" "$shortfall" "$(( 900000000 - reserve_a_after ))"

  # ── CL health: balance-based reserves, with and without a pool kind ─────
  invoke "$rm" set_min_reserve --token_a "$tc" --token_b "$td" \
    --min_reserve_a 1000 --min_reserve_b 1000 >/dev/null
  assert_eq "reserve_manager: CL pool with no balance is unhealthy" \
    "$(invoke "$rm" check_reserves --pool "$cl" | tr -d '[:space:]"')" "false"
  invoke "$tc" mint --to "$governance" --amount 100000 >/dev/null
  invoke "$td" mint --to "$governance" --amount 100000 >/dev/null
  invoke "$tc" transfer --from "$governance" --to "$cl" --amount 100000 >/dev/null
  invoke "$td" transfer --from "$governance" --to "$cl" --amount 100000 >/dev/null
  assert_eq "reserve_manager: CL pool is healthy once funded" \
    "$(invoke "$rm" check_reserves --pool "$cl" | tr -d '[:space:]"')" "true"
  invoke "$rm" set_pool_kind --pool "$cl" --kind '"ConcentratedLiquidity"' >/dev/null
  assert_contains "reserve_manager: pool kind reads back as CL" \
    "$(invoke "$rm" get_pool_kind --pool "$cl")" "ConcentratedLiquidity"

  # ── batch check over a mixed list, and the page limit ───────────────────
  local batch
  batch=$(invoke "$rm" check_reserves_batch --pools "[\"$amm\",\"$cl\"]")
  assert_eq "reserve_manager: batch returns one report per pool" "$(count_matches '"pool"' <<<"$batch")" "2"
  assert_contains "reserve_manager: batch includes the CL pool" "$batch" "$cl"

  local oversized
  oversized=$(printf '"%s",' $(for _ in $(seq 1 51); do printf '%s ' "$cl"; done) | sed 's/,$//')
  expect_fail "reserve_manager: a batch above MAX_PAGE is rejected" \
    invoke "$rm" check_reserves_batch --pools "[$oversized]"

  # ── pause blocks writes but not reads ───────────────────────────────────
  invoke "$rm" pause >/dev/null
  assert_eq "reserve_manager: is_paused reads true" "$(invoke "$rm" is_paused | tr -d '[:space:]"')" "true"
  expect_fail "reserve_manager: set_min_reserve is blocked while paused" \
    invoke "$rm" set_min_reserve --token_a "$ta" --token_b "$tb" --min_reserve_a 1 --min_reserve_b 1
  assert_eq "reserve_manager: reads still work while paused" \
    "$(invoke "$rm" check_reserves --pool "$cl" | tr -d '[:space:]"')" "true"
  invoke "$rm" unpause >/dev/null
  invoke "$rm" set_min_reserve --token_a "$ta" --token_b "$tb" \
    --min_reserve_a 100000 --min_reserve_b 200000 >/dev/null
  assert_eq "reserve_manager: writes resume after unpause" \
    "$(invoke "$rm" get_min_reserve --token_a "$ta" --token_b "$tb" | json_num min_reserve_a)" "100000"

  # ── two-step admin handover ─────────────────────────────────────────────
  invoke "$rm" propose_admin --admin "$governance" --new_admin "$nominee" >/dev/null
  assert_eq "reserve_manager: nominee is pending" \
    "$(invoke "$rm" get_pending_admin | parse_address)" "$nominee"
  expect_fail "reserve_manager: a stranger cannot accept the nomination" \
    invoke_as "$outsider_key" "$rm" accept_admin --new_admin "$outsider"
  invoke_as "${SOURCE_ACCOUNT}-rm-nominee" "$rm" accept_admin --new_admin "$nominee" >/dev/null
  assert_eq "reserve_manager: admin is the nominee after accepting" \
    "$(invoke "$rm" get_admin | parse_address)" "$nominee"
  assert_eq "reserve_manager: get_admin equals get_governance after handover" \
    "$(invoke "$rm" get_governance | parse_address)" "$nominee"
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
  # shellcheck source=scripts/e2e/common.sh
  source "$(dirname "${BASH_SOURCE[0]}")/common.sh"
  e2e_standalone run_reserve_manager_flow
fi
