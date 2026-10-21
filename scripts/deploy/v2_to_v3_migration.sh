#!/usr/bin/env bash
# v2_to_v3_migration.sh — deploy V2->V3 Migration helper
# Sourceable module for deploy.sh
#
# One instance serves every pair: it is initialized with the factory that
# attests the pools passed to `migrate`. The AMM/CL pair from this deployment
# is stored as the default pair for the legacy `migrate_default` entry point.

deploy_v2_to_v3_migration() {
  CURRENT_CONTRACT="v2_to_v3_migration"
  log "== v2_to_v3_migration =="

  local wasm
  wasm=$(wasm_path v2_to_v3_migration)

  if [[ -z "${AMM_POOL_CONTRACT_ID:-}" ]]; then AMM_POOL_CONTRACT_ID=$(get_persisted AMM_POOL_CONTRACT_ID || echo ""); fi
  if [[ -z "${CL_POOL_CONTRACT_ID:-}" ]]; then CL_POOL_CONTRACT_ID=$(get_persisted CL_POOL_CONTRACT_ID || echo ""); fi
  if [[ -z "${FACTORY_CONTRACT_ID:-}" ]]; then FACTORY_CONTRACT_ID=$(get_persisted FACTORY_CONTRACT_ID || echo ""); fi

  if [[ -z "${AMM_POOL_CONTRACT_ID:-}" || -z "${CL_POOL_CONTRACT_ID:-}" || -z "${FACTORY_CONTRACT_ID:-}" ]]; then
    warn "missing AMM, CL or factory contract — skipping v2_to_v3_migration"
    return 0
  fi

  if should_skip_persisted "V2_TO_V3_MIGRATION_CONTRACT_ID"; then
    V2_TO_V3_MIGRATION_CONTRACT_ID=$(get_persisted V2_TO_V3_MIGRATION_CONTRACT_ID)
    log "skipping v2_to_v3_migration deploy (already at $V2_TO_V3_MIGRATION_CONTRACT_ID)"
  else
    if ! should_deploy "v2_to_v3_migration"; then
      log "skipping v2_to_v3_migration deploy (--only/--skip filter)"
      V2_TO_V3_MIGRATION_CONTRACT_ID=$(get_persisted V2_TO_V3_MIGRATION_CONTRACT_ID || echo "")
      if [[ -z "$V2_TO_V3_MIGRATION_CONTRACT_ID" ]]; then return 0; fi
    else
      CURRENT_STEP="deploy v2_to_v3_migration"
      log "deploying v2_to_v3_migration: $wasm"
      if [[ ! -f "$wasm" ]]; then
        warn "WASM not found: $wasm — skipping"
        return 0
      fi
      V2_TO_V3_MIGRATION_CONTRACT_ID=$(deploy_contract "$wasm")
      persist_var "V2_TO_V3_MIGRATION_CONTRACT_ID" "$V2_TO_V3_MIGRATION_CONTRACT_ID"
      log "v2_to_v3_migration: $V2_TO_V3_MIGRATION_CONTRACT_ID"
    fi
  fi

  export V2_TO_V3_MIGRATION_CONTRACT_ID
  if [[ -z "${V2_TO_V3_MIGRATION_CONTRACT_ID:-}" ]]; then return 0; fi

  if should_skip_persisted "V2_TO_V3_MIGRATION_INITIALIZED"; then
    log "skipping v2_to_v3_migration initialize (already done)"
  else
    if ! should_deploy "v2_to_v3_migration"; then
      log "skipping v2_to_v3_migration initialize (--only/--skip filter)"
    else
      CURRENT_STEP="initialize v2_to_v3_migration"
      log "initializing v2_to_v3_migration admin=$ADMIN_ADDRESS factory=$FACTORY_CONTRACT_ID"
      # The contract exposes no getters to read the init state back, so a
      # failed initialize is fatal rather than assumed to be a rerun.
      if ! invoke "$V2_TO_V3_MIGRATION_CONTRACT_ID" initialize --admin "$ADMIN_ADDRESS" --factory "$FACTORY_CONTRACT_ID" >/dev/null 2>&1; then
        die "failed to initialize v2_to_v3_migration at $V2_TO_V3_MIGRATION_CONTRACT_ID"
      fi
      log "v2_to_v3_migration initialized"
      persist_var "V2_TO_V3_MIGRATION_INITIALIZED" "1"
    fi
  fi

  if should_skip_persisted "V2_TO_V3_MIGRATION_DEFAULT_PAIR_SET"; then
    log "skipping v2_to_v3_migration default pair (already set)"
  elif should_deploy "v2_to_v3_migration"; then
    CURRENT_STEP="set default pair v2_to_v3_migration"
    log "setting v2_to_v3_migration default pair v2=$AMM_POOL_CONTRACT_ID v3=$CL_POOL_CONTRACT_ID"
    if ! invoke "$V2_TO_V3_MIGRATION_CONTRACT_ID" set_default_pair --v2_pool "$AMM_POOL_CONTRACT_ID" --v3_pool "$CL_POOL_CONTRACT_ID" >/dev/null 2>&1; then
      die "failed to set default pair on v2_to_v3_migration at $V2_TO_V3_MIGRATION_CONTRACT_ID"
    fi
    persist_var "V2_TO_V3_MIGRATION_DEFAULT_PAIR_SET" "1"
  fi
}
