#!/usr/bin/env bash
# governance.sh — deploy and initialize Governance contract
# Sourceable module for deploy.sh

deploy_governance() {
  CURRENT_CONTRACT="governance"
  log "== governance =="

  local gov_wasm
  gov_wasm=$(wasm_path governance)

  if [[ -z "${AMM_POOL_CONTRACT_ID:-}" ]]; then AMM_POOL_CONTRACT_ID=$(get_persisted AMM_POOL_CONTRACT_ID || echo ""); fi
  if [[ -z "${LP_TOKEN_CONTRACT_ID:-}" ]]; then LP_TOKEN_CONTRACT_ID=$(get_persisted LP_TOKEN_CONTRACT_ID || echo ""); fi

  if [[ -z "${AMM_POOL_CONTRACT_ID:-}" || -z "${LP_TOKEN_CONTRACT_ID:-}" ]]; then
    warn "missing AMM pool or LP token — deferring governance deploy"
    # Try to discover from factory if available
    if [[ -n "${FACTORY_CONTRACT_ID:-}" ]]; then
      local disc_pool
      disc_pool=$(invoke_read "$FACTORY_CONTRACT_ID" get_pool --token_a "$TOKEN_A_CONTRACT_ID" --token_b "$TOKEN_B_CONTRACT_ID" 2>/dev/null | grep -Eo 'C[A-Z0-9]{55}' | tail -n 1 || echo "")
      if [[ -n "$disc_pool" ]]; then
        AMM_POOL_CONTRACT_ID="$disc_pool"
        LP_TOKEN_CONTRACT_ID=$(invoke_read "$FACTORY_CONTRACT_ID" get_lp_token --pool "$disc_pool" 2>/dev/null | grep -Eo 'C[A-Z0-9]{55}' | tail -n 1 || echo "")
        export AMM_POOL_CONTRACT_ID LP_TOKEN_CONTRACT_ID
      fi
    fi
    if [[ -z "${AMM_POOL_CONTRACT_ID:-}" ]]; then
      warn "still missing AMM pool — skipping governance"
      return 0
    fi
  fi

  if should_skip_persisted "GOVERNANCE_CONTRACT_ID"; then
    GOVERNANCE_CONTRACT_ID=$(get_persisted GOVERNANCE_CONTRACT_ID)
    log "skipping governance deploy (already at $GOVERNANCE_CONTRACT_ID)"
  else
    if ! should_deploy "governance"; then
      log "skipping governance deploy (--only/--skip filter)"
      GOVERNANCE_CONTRACT_ID=$(get_persisted GOVERNANCE_CONTRACT_ID || echo "")
      if [[ -z "$GOVERNANCE_CONTRACT_ID" ]]; then return 0; fi
    else
      CURRENT_STEP="deploy governance"
      log "deploying governance: $gov_wasm"
      if [[ ! -f "$gov_wasm" ]]; then
        warn "governance WASM not found: $gov_wasm — skipping"
        return 0
      fi
      GOVERNANCE_CONTRACT_ID=$(deploy_contract "$gov_wasm")
      persist_var "GOVERNANCE_CONTRACT_ID" "$GOVERNANCE_CONTRACT_ID"
      log "governance: $GOVERNANCE_CONTRACT_ID"
    fi
  fi

  export GOVERNANCE_CONTRACT_ID

  if [[ -z "${GOVERNANCE_CONTRACT_ID:-}" ]]; then return 0; fi

  if should_skip_persisted "GOVERNANCE_INITIALIZED"; then
    log "skipping governance initialize (already done)"
  else
    if ! should_deploy "governance"; then
      log "skipping governance initialize (--only/--skip filter)"
    else
      CURRENT_STEP="initialize governance"
      log "initializing governance: amm=$AMM_POOL_CONTRACT_ID lp=$LP_TOKEN_CONTRACT_ID voting=$DEFAULT_VOTING_PERIOD_SECS timelock=$DEFAULT_TIMELOCK_SECS quorum=$DEFAULT_QUORUM_BPS stake=$DEFAULT_MIN_PROPOSER_STAKE_BPS"
      if ! invoke "$GOVERNANCE_CONTRACT_ID" initialize \
          --admin "$ADMIN_ADDRESS" \
          --amm_pool "$AMM_POOL_CONTRACT_ID" \
          --lp_token "$LP_TOKEN_CONTRACT_ID" \
          --voting_period_secs "$DEFAULT_VOTING_PERIOD_SECS" \
          --timelock_secs "$DEFAULT_TIMELOCK_SECS" \
          --quorum_bps "$DEFAULT_QUORUM_BPS" \
          --min_proposer_stake_bps "$DEFAULT_MIN_PROPOSER_STAKE_BPS" >/dev/null 2>&1; then
        if invoke_read "$GOVERNANCE_CONTRACT_ID" get_params >/dev/null 2>/dev/null; then
          log "governance already initialized"
        else
          die "failed to initialize governance"
        fi
      else
        log "governance initialized"
      fi
      persist_var "GOVERNANCE_INITIALIZED" "1"
    fi
  fi

  # Runs on every deploy, not only the first: it is idempotent, and a rerun
  # must still fail if an earlier run left the locker unwired.
  if should_deploy "governance"; then
    wire_lp_locker "$AMM_POOL_CONTRACT_ID" "$LP_TOKEN_CONTRACT_ID" "$GOVERNANCE_CONTRACT_ID"
  fi

  CURRENT_STEP="verify governance"
  if ! verify_governance "$GOVERNANCE_CONTRACT_ID" "$AMM_POOL_CONTRACT_ID" "$LP_TOKEN_CONTRACT_ID" "$ADMIN_ADDRESS"; then
    warn "governance verification warning"
  else
    log "verified governance points at correct pool/lp"
  fi
}

# Point the LP token's locker at governance, then read it back. vote() locks
# LP tokens through LpToken::lock, which the locker must authorise, so a
# governance deployment whose locker is anything else cannot record a single
# vote (issue #986). Exits the deploy on failure.
#
# Only the pool can change the locker (it is the LP token's admin), so the
# change always goes through the pool's admin-gated set_lp_locker:
# - pool admin is this account: call set_lp_locker directly;
# - pool admin is governance (factory create_pool with governance): ask
#   governance to call it via claim_lp_locker.
wire_lp_locker() {
  local pool="$1" lp="$2" gov="$3"
  local current
  CURRENT_STEP="read LP token locker"
  current=$(invoke_read "$lp" locker | extract_contract_id || true)
  if [[ "$current" == "$gov" ]]; then
    log "LP token locker already set to governance"
    return 0
  fi

  CURRENT_STEP="set LP token locker to governance"
  log "setting LP token locker to governance (was ${current:-<unknown>})"
  local out
  if ! out=$(invoke "$pool" set_lp_locker --locker "$gov" 2>&1); then
    log "pool set_lp_locker as $SOURCE_PUBLIC_KEY failed, trying governance claim_lp_locker: $out"
    if ! out=$(invoke "$gov" claim_lp_locker 2>&1); then
      die "could not set LP token locker to governance: neither this account nor governance is the pool admin: $out"
    fi
  fi

  CURRENT_STEP="verify LP token locker"
  current=$(invoke_read "$lp" locker | extract_contract_id || true)
  if [[ "$current" != "$gov" ]]; then
    die "LP token locker is ${current:-<unreadable>} after wiring, expected governance $gov — vote() would trap"
  fi
  log "verified LP token locker is governance"
}
