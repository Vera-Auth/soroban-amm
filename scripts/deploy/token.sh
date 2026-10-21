#!/usr/bin/env bash
# token.sh — deploy underlying asset tokens (Token A, Token B, Reward Token)
# Sourceable module for deploy.sh

deploy_tokens() {
  CURRENT_CONTRACT="token"
  log "== tokens =="

  local token_wasm
  token_wasm=$(wasm_path token)
  if [[ ! -f "$token_wasm" ]]; then
    # try hyphen variant
    token_wasm="${token_wasm//_/-}"
  fi
  if [[ ! -f "$token_wasm" ]]; then
    die "token WASM not found at $token_wasm — run cargo build --release --target ${WASM_TARGET} first"
  fi
  check_wasm_size "$token_wasm" "token"

  # ── Token A: deploy then initialize immediately (closes front-run window) ──
  if should_skip_persisted "TOKEN_A_CONTRACT_ID"; then
    TOKEN_A_CONTRACT_ID=$(get_persisted TOKEN_A_CONTRACT_ID)
    log "skipping Token A deploy (already at $TOKEN_A_CONTRACT_ID, use --force to redeploy)"
  else
    if ! should_deploy "token"; then
      log "skipping Token A (--only/--skip filter)"
      TOKEN_A_CONTRACT_ID=$(get_persisted TOKEN_A_CONTRACT_ID || echo "")
    else
      CURRENT_STEP="deploy Token A"
      log "deploying Token A"
      TOKEN_A_CONTRACT_ID=$(deploy_contract "$token_wasm")
      persist_var "TOKEN_A_CONTRACT_ID" "$TOKEN_A_CONTRACT_ID"
      log "Token A deployed: $TOKEN_A_CONTRACT_ID"

      # Initialize in the very next transaction to close the front-run window.
      # token::initialize has no authorization check — anyone who calls it first
      # on a freshly deployed contract becomes admin.  We call it immediately
      # after deploy before any other transaction can land.
      CURRENT_STEP="initialize Token A"
      log "initializing Token A (immediately after deploy)"
      invoke "$TOKEN_A_CONTRACT_ID" initialize \
        --admin "$SOURCE_PUBLIC_KEY" \
        --name "Soroban AMM Token A" \
        --symbol "SAMA" \
        --decimals 7 >/dev/null 2>&1 || {
          if invoke_read "$TOKEN_A_CONTRACT_ID" name >/dev/null 2>/dev/null; then
            log "Token A already initialized (verified via name read)"
          else
            die "failed to initialize Token A"
          fi
        }
      persist_var "TOKEN_A_INITIALIZED" "1"
      # Verification is now fatal: a mismatch means the contract was front-run.
      verify_token "$TOKEN_A_CONTRACT_ID" "$SOURCE_PUBLIC_KEY"
      log "Token A: $TOKEN_A_CONTRACT_ID"
    fi
  fi

  # ── Token B: deploy then initialize immediately ──────────────────────────
  if should_skip_persisted "TOKEN_B_CONTRACT_ID"; then
    TOKEN_B_CONTRACT_ID=$(get_persisted TOKEN_B_CONTRACT_ID)
    log "skipping Token B deploy (already at $TOKEN_B_CONTRACT_ID)"
  else
    if ! should_deploy "token"; then
      log "skipping Token B (--only/--skip filter)"
      TOKEN_B_CONTRACT_ID=$(get_persisted TOKEN_B_CONTRACT_ID || echo "")
    else
      CURRENT_STEP="deploy Token B"
      log "deploying Token B"
      TOKEN_B_CONTRACT_ID=$(deploy_contract "$token_wasm")
      persist_var "TOKEN_B_CONTRACT_ID" "$TOKEN_B_CONTRACT_ID"
      log "Token B deployed: $TOKEN_B_CONTRACT_ID"

      CURRENT_STEP="initialize Token B"
      log "initializing Token B (immediately after deploy)"
      invoke "$TOKEN_B_CONTRACT_ID" initialize \
        --admin "$SOURCE_PUBLIC_KEY" \
        --name "Soroban AMM Token B" \
        --symbol "SAMB" \
        --decimals 7 >/dev/null 2>&1 || {
          if invoke_read "$TOKEN_B_CONTRACT_ID" name >/dev/null 2>/dev/null; then
            log "Token B already initialized"
          else
            die "failed to initialize Token B"
          fi
        }
      persist_var "TOKEN_B_INITIALIZED" "1"
      verify_token "$TOKEN_B_CONTRACT_ID" "$SOURCE_PUBLIC_KEY"
      log "Token B: $TOKEN_B_CONTRACT_ID"
    fi
  fi

  # ── Reward Token: deploy then initialize immediately ─────────────────────
  if should_skip_persisted "REWARD_TOKEN_CONTRACT_ID"; then
    REWARD_TOKEN_CONTRACT_ID=$(get_persisted REWARD_TOKEN_CONTRACT_ID)
    log "skipping Reward Token deploy (already at $REWARD_TOKEN_CONTRACT_ID)"
  else
    if ! should_deploy "token"; then
      log "skipping Reward Token (--only/--skip filter)"
      REWARD_TOKEN_CONTRACT_ID=$(get_persisted REWARD_TOKEN_CONTRACT_ID || echo "")
    else
      CURRENT_STEP="deploy Reward Token"
      log "deploying Reward Token"
      REWARD_TOKEN_CONTRACT_ID=$(deploy_contract "$token_wasm")
      persist_var "REWARD_TOKEN_CONTRACT_ID" "$REWARD_TOKEN_CONTRACT_ID"
      log "Reward Token deployed: $REWARD_TOKEN_CONTRACT_ID"

      CURRENT_STEP="initialize Reward Token"
      log "initializing Reward Token (immediately after deploy)"
      # A public RPC node can lag the deploy by a few seconds, so an initialize
      # sent right after it is rejected as "not found". A rejected initialize
      # has not been applied, so it is safe to send it again after a short wait.
      reward_initialized=0
      for attempt in 1 2 3 4; do
        if invoke "$REWARD_TOKEN_CONTRACT_ID" initialize \
          --admin "$SOURCE_PUBLIC_KEY" \
          --name "Soroban AMM Reward" \
          --symbol "SAMR" \
          --decimals 7 >/dev/null 2>&1; then
          reward_initialized=1
          break
        fi
        if invoke_read "$REWARD_TOKEN_CONTRACT_ID" name >/dev/null 2>/dev/null; then
          log "Reward Token already initialized"
          reward_initialized=1
          break
        fi
        log "initialize Reward Token not accepted yet (attempt $attempt of 4); waiting before retrying"
        sleep $(( attempt * 10 ))
      done
      if [[ "$reward_initialized" -ne 1 ]]; then
        die "failed to initialize Reward Token"
      fi
      persist_var "REWARD_TOKEN_INITIALIZED" "1"
      verify_token "$REWARD_TOKEN_CONTRACT_ID" "$SOURCE_PUBLIC_KEY"
      log "Reward Token: $REWARD_TOKEN_CONTRACT_ID"
    fi
  fi

  # Export for downstream modules
  export TOKEN_A_CONTRACT_ID TOKEN_B_CONTRACT_ID REWARD_TOKEN_CONTRACT_ID
}
