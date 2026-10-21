#!/usr/bin/env bash
# run.sh — e2e test orchestrator. Deploys fresh contracts via scripts/deploy.sh,
# then runs each flow module in scripts/e2e/, reporting a summary at the end.
#
# Usage:
#   bash scripts/e2e/run.sh                  # run every flow
#   bash scripts/e2e/run.sh --only v2,factory
#   bash scripts/e2e/run.sh --skip factory
#
# Two kinds of flow:
#   - shared flows (v2 factory cl governance staking twap) run against the
#     addresses deploy.sh writes to $DEPLOY_ENV;
#   - self-contained flows (token router dex_aggregator oracle_aggregator
#     cl_position_nft pol_vesting twap_consumer) deploy their own instances
#     and can also be run directly, e.g. `bash scripts/e2e/router.sh`.
# deploy.sh only runs when at least one selected flow is a shared flow.
#
# A failing flow does not prevent the others from running; the script exits
# non-zero if any flow failed, after every flow has had a chance to run.
set -Eeuo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

# shellcheck source=scripts/e2e/common.sh
source "$ROOT_DIR/e2e/common.sh"

SHARED_FLOWS=(v2 factory cl governance staking twap)
SELF_CONTAINED_FLOWS=(token router dex_aggregator oracle_aggregator cl_position_nft pol_vesting twap_consumer twal_consumer)
ALL_FLOWS=("${SHARED_FLOWS[@]}" "${SELF_CONTAINED_FLOWS[@]}")

ONLY_RAW=""
SKIP_RAW=""

print_help() {
  cat <<HELP
Soroban AMM e2e test suite

Usage:
  bash scripts/e2e/run.sh [options]

Options:
  --only LIST    Run only these flows (comma-separated). Available: ${ALL_FLOWS[*]}
  --skip LIST    Skip these flows (comma-separated).
  --help, -h     Show this help.
HELP
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --only) ONLY_RAW="$2"; shift 2 ;;
    --skip) SKIP_RAW="$2"; shift 2 ;;
    --help|-h) print_help; exit 0 ;;
    *) echo "unknown argument: $1" >&2; print_help; exit 2 ;;
  esac
done

should_run_flow() {
  local flow="$1"
  if [[ -n "$ONLY_RAW" ]]; then
    [[ ",${ONLY_RAW}," == *",${flow},"* ]]
    return
  fi
  if [[ -n "$SKIP_RAW" ]]; then
    [[ ",${SKIP_RAW}," != *",${flow},"* ]]
    return
  fi
  return 0
}

needs_shared_deploy() {
  local flow
  for flow in "${SHARED_FLOWS[@]}"; do
    if should_run_flow "$flow"; then
      return 0
    fi
  done
  return 1
}

require_cmd stellar

generate_and_fund_source
SOURCE_PUBLIC_KEY="$(stellar keys address "$SOURCE_ACCOUNT")"
export NETWORK SOURCE_ACCOUNT SOURCE_PUBLIC_KEY DEPLOY_ENV

if needs_shared_deploy; then
  if "$ROOT_DIR/deploy.sh" >/dev/null; then
    pass "deployed and initialized fresh contracts"
  else
    die "deploy script failed"
  fi

  # shellcheck disable=SC1090
  source "$DEPLOY_ENV"
  export TOKEN_A_CONTRACT_ID TOKEN_B_CONTRACT_ID AMM_CONTRACT_ID FACTORY_CONTRACT_ID
  export AMM_WASM_HASH TOKEN_WASM_HASH
  export AMM_POOL_CONTRACT_ID LP_TOKEN_CONTRACT_ID REWARD_TOKEN_CONTRACT_ID
  export CL_POOL_CONTRACT_ID GOVERNANCE_CONTRACT_ID STAKING_CONTRACT_ID
fi

# shellcheck source=scripts/e2e/v2.sh
source "$ROOT_DIR/e2e/v2.sh"
# shellcheck source=scripts/e2e/factory.sh
source "$ROOT_DIR/e2e/factory.sh"
# shellcheck source=scripts/e2e/cl.sh
source "$ROOT_DIR/e2e/cl.sh"
# shellcheck source=scripts/e2e/governance.sh
source "$ROOT_DIR/e2e/governance.sh"
# shellcheck source=scripts/e2e/staking.sh
source "$ROOT_DIR/e2e/staking.sh"
# shellcheck source=scripts/e2e/twap.sh
source "$ROOT_DIR/e2e/twap.sh"
# shellcheck source=scripts/e2e/token.sh
source "$ROOT_DIR/e2e/token.sh"
# shellcheck source=scripts/e2e/router.sh
source "$ROOT_DIR/e2e/router.sh"
# shellcheck source=scripts/e2e/dex_aggregator.sh
source "$ROOT_DIR/e2e/dex_aggregator.sh"
# shellcheck source=scripts/e2e/oracle_aggregator.sh
source "$ROOT_DIR/e2e/oracle_aggregator.sh"
# shellcheck source=scripts/e2e/cl_position_nft.sh
source "$ROOT_DIR/e2e/cl_position_nft.sh"
# shellcheck source=scripts/e2e/pol_vesting.sh
source "$ROOT_DIR/e2e/pol_vesting.sh"
# shellcheck source=scripts/e2e/twap_consumer.sh
source "$ROOT_DIR/e2e/twap_consumer.sh"
# shellcheck source=scripts/e2e/twal_consumer.sh
source "$ROOT_DIR/e2e/twal_consumer.sh"

declare -A FLOW_STATUS
declare -A FLOW_DURATION

# Each flow runs in a subshell wrapper so a `die` in one flow (which calls
# `exit 1`) stops only that flow — the rest still run and the summary still
# reports every flow's result.
run_flow_isolated() {
  local flow="$1"
  local fn="$2"
  local start end rc=0

  if ! should_run_flow "$flow"; then
    FLOW_STATUS["$flow"]="skipped"
    return
  fi

  start=$(date +%s)
  set +e
  (
    set -Eeuo pipefail
    e2e_trap_errors
    "$fn"
  )
  rc=$?
  set -e
  end=$(date +%s)
  FLOW_DURATION["$flow"]=$(( end - start ))
  FLOW_STATUS["$flow"]=$([[ $rc -eq 0 ]] && echo "pass" || echo "fail")
}

run_flow_isolated v2 run_v2_flow
run_flow_isolated factory run_factory_flow
run_flow_isolated cl run_cl_flow
run_flow_isolated governance run_governance_flow
run_flow_isolated staking run_staking_flow
run_flow_isolated twap run_twap_flow
run_flow_isolated token run_token_flow
run_flow_isolated router run_router_flow
run_flow_isolated dex_aggregator run_dex_aggregator_flow
run_flow_isolated oracle_aggregator run_oracle_aggregator_flow
run_flow_isolated cl_position_nft run_cl_position_nft_flow
run_flow_isolated pol_vesting run_pol_vesting_flow
run_flow_isolated twap_consumer run_twap_consumer_flow
run_flow_isolated twal_consumer run_twal_consumer_flow

printf '\n%s\n' "Summary"
printf '%s\n' "-------"
overall_rc=0
for flow in "${ALL_FLOWS[@]}"; do
  status="${FLOW_STATUS[$flow]:-skipped}"
  duration="${FLOW_DURATION[$flow]:-0}"
  printf '%-18s %-8s %ss\n' "$flow" "$status" "$duration"
  if [[ "$status" == "fail" ]]; then
    overall_rc=1
  fi
done

if [[ $overall_rc -eq 0 ]]; then
  printf '\nE2E PASS\n'
else
  printf '\nE2E FAIL\n' >&2
fi
exit $overall_rc
