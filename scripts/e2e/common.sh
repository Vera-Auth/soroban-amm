#!/usr/bin/env bash
# common.sh — shared helpers for the e2e test flows in scripts/e2e/*.sh
# Sourceable only; no side effects beyond defining functions/vars.

ROOT_DIR="${ROOT_DIR:-$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)}"
NETWORK="${STELLAR_NETWORK:-${NETWORK:-testnet}}"
SOURCE_ACCOUNT="${SOURCE_ACCOUNT:-soroban-amm-e2e-$(date +%s)}"
DEPLOY_ENV="${DEPLOY_ENV:-"$ROOT_DIR/.soroban-amm.e2e.env"}"

# Command substitutions inherit `set -e`, so a failing call inside a helper
# like `id=$(e2e_new_token X)` fails the caller instead of being swallowed.
shopt -s inherit_errexit 2>/dev/null || true

PASS_COUNT=0
FAIL_COUNT=0
CURRENT_FLOW=""

pass() {
  PASS_COUNT=$((PASS_COUNT + 1))
  printf '[PASS] %s\n' "$*"
}

fail() {
  FAIL_COUNT=$((FAIL_COUNT + 1))
  printf '[FAIL] %s\n' "$*" >&2
}

die() {
  fail "$*"
  exit 1
}

require_cmd() {
  if ! command -v "$1" >/dev/null 2>&1; then
    die "missing required command: $1"
  fi
}

invoke() {
  local contract_id="$1"
  shift
  invoke_as "$SOURCE_ACCOUNT" "$contract_id" "$@"
}

parse_i128() {
  grep -Eo -- '-?[0-9]+' | tail -n 1
}

# field_value FIELD — last numeric value of FIELD in stdin. The CLI prints
# i128/u128 values as quoted JSON strings, so the quote is optional.
field_value() {
  local field="$1"
  grep -Eo "\"?${field}\"?[[:space:]]*[:=][[:space:]]*\"?-?[0-9]+" | grep -Eo -- '-?[0-9]+' | tail -n 1
}

extract_contract_id() {
  grep -Eo 'C[A-Z0-9]{55}' | tail -n 1
}

assert_eq() {
  local label="$1"
  local actual="$2"
  local expected="$3"

  if [[ "$actual" == "$expected" ]]; then
    pass "$label: $actual"
  else
    die "$label: expected $expected, got $actual"
  fi
}

assert_between() {
  local label="$1"
  local actual="$2"
  local min="$3"
  local max="$4"

  if [[ ! "$actual" =~ ^-?[0-9]+$ ]]; then
    die "$label: expected numeric value, got '$actual'"
  fi

  if (( actual >= min && actual <= max )); then
    pass "$label: $actual is within [$min, $max]"
  else
    die "$label: expected value within [$min, $max], got $actual"
  fi
}

assert_gt() {
  local label="$1"
  local actual="$2"
  local floor="$3"

  if [[ ! "$actual" =~ ^-?[0-9]+$ ]]; then
    die "$label: expected numeric value, got '$actual'"
  fi
  if (( actual > floor )); then
    pass "$label: $actual > $floor"
  else
    die "$label: expected > $floor, got $actual"
  fi
}

assert_lte_abs() {
  local label="$1"
  local actual="$2"
  local max_abs="$3"
  local abs="$actual"

  if [[ ! "$actual" =~ ^-?[0-9]+$ ]]; then
    die "$label: expected numeric value, got '$actual'"
  fi
  if (( abs < 0 )); then
    abs=$(( -abs ))
  fi
  if (( abs <= max_abs )); then
    pass "$label: $actual <= dust limit $max_abs"
  else
    die "$label: expected <= $max_abs dust, got $actual"
  fi
}

generate_and_fund_source() {
  if stellar keys address "$SOURCE_ACCOUNT" >/dev/null 2>&1; then
    pass "source account exists: $SOURCE_ACCOUNT"
    return
  fi

  if stellar keys generate "$SOURCE_ACCOUNT" --network "$NETWORK" --fund >/dev/null 2>&1; then
    pass "generated and funded source account: $SOURCE_ACCOUNT"
    return
  fi

  stellar keys generate --default-seed "$SOURCE_ACCOUNT" >/dev/null
  stellar keys fund "$SOURCE_ACCOUNT" --network "$NETWORK" >/dev/null
  pass "generated and funded source account: $SOURCE_ACCOUNT"
}

# ── Self-contained fixtures ─────────────────────────────────────────────────
# Helpers for flows that deploy their own contract instances instead of
# reading shared addresses from deploy.sh's env file. A flow built on these
# can run on its own (`bash scripts/e2e/router.sh`) without a full protocol
# deployment, and cannot be disturbed by state other flows leave behind.

E2E_REPO_ROOT="${E2E_REPO_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)}"
E2E_WASM_DIR="${E2E_WASM_DIR:-$E2E_REPO_ROOT/target/wasm32v1-none/release}"

# parse_address — last Stellar account (G…) or contract (C…) strkey in stdin.
parse_address() {
  grep -Eo '[GC][A-Z2-7]{55}' | tail -n 1
}

# json_num FIELD — first numeric value of FIELD in stellar CLI JSON output.
# Tolerates i128/u128 values, which the CLI prints as quoted strings.
json_num() {
  local field="$1"
  grep -Eo "\"${field}\"[[:space:]]*:[[:space:]]*\"?-?[0-9]+" | head -n 1 | grep -Eo -- '-?[0-9]+$'
}

# nth_num N — the Nth (1-based) integer in stdin, e.g. an element of a
# returned Vec<i128> or tuple.
nth_num() {
  grep -Eo -- '-?[0-9]+' | sed -n "${1}p"
}

# count_matches PATTERN — number of occurrences of PATTERN in stdin.
count_matches() {
  { grep -Eo -- "$1" || true; } | wc -l | tr -d '[:space:]'
}

assert_contains() {
  local label="$1"
  local haystack="$2"
  local needle="$3"

  if [[ "$haystack" == *"$needle"* ]]; then
    pass "$label"
  else
    die "$label: expected output to contain $needle, got: $haystack"
  fi
}

assert_not_contains() {
  local label="$1"
  local haystack="$2"
  local needle="$3"

  if [[ "$haystack" != *"$needle"* ]]; then
    pass "$label"
  else
    die "$label: expected output not to contain $needle, got: $haystack"
  fi
}

# invoke_as ACCOUNT CONTRACT_ID FN ARGS... — invoke with a different signer.
#
# Simulation failing gets one more attempt:
#
# - The public RPC endpoint is load-balanced, and the node
#   that simulates can be a ledger behind the one that confirmed the caller's
#   previous transaction, so a call that spends what that transaction minted
#   sees the old balance. The retry waits a ledger first. A genuine failure
#   fails again; expect_fail turns this off (E2E_SIM_RETRY=0) so negative
#   checks don't pay for the wait.
invoke_as() {
  local account="$1"
  local contract_id="$2"
  shift 2
  local err out rc attempt
  err=$(mktemp)
  for attempt in 1 2; do
    rc=0
    out=$(stellar contract invoke \
      --id "$contract_id" \
      --network "$NETWORK" \
      --source "$account" \
      -- "$@" 2>"$err") || rc=$?
    cat "$err" >&2
    if [[ "$rc" -eq 0 || "$attempt" -eq 2 ]]; then
      break
    fi
    if [[ "${E2E_SIM_RETRY:-1}" == "1" ]] && grep -q 'transaction simulation failed' "$err"; then
      printf '[retry] %s simulation failed; waiting a ledger in case the RPC node lags, then retrying\n' "$1" >&2
      sleep 6
    elif grep -q 'Account not found' "$err"; then
      # Rejected before submission, so repeating it cannot apply twice.
      printf '[retry] %s: RPC node has not seen the account yet; waiting, then retrying\n' "$1" >&2
      sleep 10
    else
      break
    fi
  done
  rm -f "$err"
  printf '%s\n' "$out"
  return "$rc"
}

# invoke_signed_only_by ACCOUNT CONTRACT_ID FN ARGS... — like invoke_as, but
# the transaction carries ACCOUNT's signatures only. invoke and invoke_as let
# the CLI sign every auth entry whose address has a key in the local keystore,
# which includes the admin, pool and keeper keys these flows create, so a
# "rejected without X's auth" check made through them passes or fails
# regardless of what the contract enforces. Use this for those checks.
invoke_signed_only_by() {
  local -
  set -o pipefail
  local account="$1"
  local contract_id="$2"
  shift 2
  stellar contract invoke \
    --id "$contract_id" \
    --network "$NETWORK" \
    --source "$account" \
    --build-only \
    -- "$@" \
    | stellar tx simulate --network "$NETWORK" --source "$account" \
    | stellar tx sign --network "$NETWORK" --sign-with-key "$account" \
    | stellar tx send --network "$NETWORK"
}

# expect_fail LABEL CMD... — CMD must exit non-zero (e.g. an auth or
# slippage rejection); a success is a test failure.
expect_fail() {
  local label="$1"
  shift
  local out rc=0
  out=$(trap - ERR; E2E_SIM_RETRY=0 "$@" 2>&1) || rc=$?
  if [[ "$rc" -eq 0 ]]; then
    die "$label: expected the call to be rejected, but it succeeded: $out"
  fi
  pass "$label"
}

# e2e_wasm NAME — path to a built contract artifact.
e2e_wasm() {
  local path="$E2E_WASM_DIR/$1.wasm"
  if [[ ! -f "$path" ]]; then
    die "missing WASM artifact $path — run: cargo build --release --target wasm32v1-none"
  fi
  printf '%s' "$path"
}

# e2e_retry LABEL CMD... — run CMD and print its output. A transient RPC
# failure (an account the node has not seen yet, a timeout, or a 429/5xx) is
# retried up to six times with a growing wait. Only use it for calls that are
# safe to repeat: uploads, and deploys of fresh instances.
e2e_retry() {
  local label="$1" out rc attempt
  local max_attempts=6
  shift
  for attempt in $(seq 1 "$max_attempts"); do
    rc=0
    out=$("$@" 2>&1) || rc=$?
    if [[ "$rc" -eq 0 ]]; then
      printf '%s\n' "$out"
      return 0
    fi
    # A here-string rather than a pipe: under pipefail, grep -q exiting early
    # can make the printf on the left look like a failure.
    if [[ "$attempt" -eq "$max_attempts" ]] || ! grep -qE 'Account not found|Request timeout|timed out|502|503|429' <<<"$out"; then
      printf '%s\n' "$out"
      return "$rc"
    fi
    printf '[retry] %s: transient RPC failure (attempt %s of %s); waiting before retrying\n' "$label" "$attempt" "$max_attempts" >&2
    sleep $(( attempt * 10 ))
  done
}

# e2e_deploy NAME — deploy a fresh instance of contract NAME, print its id.
e2e_deploy() {
  local wasm out id
  wasm=$(e2e_wasm "$1")
  out=$(e2e_retry "deploy of $1" stellar contract deploy \
    --wasm "$wasm" \
    --network "$NETWORK" \
    --source "$SOURCE_ACCOUNT") || die "deploy of $1 failed: $out"
  id=$(printf '%s\n' "$out" | extract_contract_id)
  if [[ -z "$id" ]]; then
    die "could not parse contract id from deploy of $1: $out"
  fi
  printf '%s' "$id"
}

# e2e_upload NAME — upload contract NAME's WASM, print its hash.
e2e_upload() {
  local wasm out hash
  wasm=$(e2e_wasm "$1")
  out=$(e2e_retry "upload of $1" stellar contract upload \
    --wasm "$wasm" \
    --network "$NETWORK" \
    --source "$SOURCE_ACCOUNT") || die "upload of $1 failed: $out"
  hash=$(printf '%s\n' "$out" | grep -Eo '[0-9a-fA-F]{64}' | tail -n 1)
  if [[ -z "$hash" ]]; then
    die "could not parse wasm hash from upload of $1: $out"
  fi
  printf '%s' "$hash"
}

# e2e_fund_account SUFFIX — generate and fund an extra signer, print its
# public key. The key is named "$SOURCE_ACCOUNT-SUFFIX".
e2e_fund_account() {
  local name="${SOURCE_ACCOUNT}-$1"
  if ! stellar keys address "$name" >/dev/null 2>&1; then
    if ! stellar keys generate "$name" --network "$NETWORK" --fund >/dev/null 2>&1; then
      stellar keys generate "$name" >/dev/null
      stellar keys fund "$name" --network "$NETWORK" >/dev/null
    fi
  fi
  stellar keys address "$name"
}

# e2e_new_token SYMBOL — deploy and initialize a fresh token whose admin is
# the source account, print its id.
e2e_new_token() {
  local symbol="$1"
  local id
  id=$(e2e_deploy token)
  invoke "$id" initialize \
    --admin "$SOURCE_PUBLIC_KEY" \
    --name "E2E $symbol" \
    --symbol "$symbol" \
    --decimals 7 >/dev/null
  printf '%s' "$id"
}

# e2e_new_factory — deploy a fresh factory administered by the source
# account, with the AMM and LP-token WASM registered. Prints its id.
e2e_new_factory() {
  local amm_hash token_hash id
  amm_hash=$(e2e_upload amm)
  token_hash=$(e2e_upload token)
  id=$(e2e_deploy factory)
  invoke "$id" initialize \
    --admin "$SOURCE_PUBLIC_KEY" \
    --amm_wasm_hash "$amm_hash" \
    --token_wasm_hash "$token_hash" >/dev/null
  printf '%s' "$id"
}

# e2e_new_seeded_pool FACTORY TOKEN_X TOKEN_Y AMOUNT — create a 30 bps AMM
# pool through FACTORY and seed it with AMOUNT of each token. Equal amounts
# keep the seed independent of the factory's token-order normalisation.
# Prints the pool id.
e2e_new_seeded_pool() {
  local factory="$1"
  local tx="$2"
  local ty="$3"
  local amount="$4"
  local out pool deadline

  out=$(invoke "$factory" create_pool_with_fee_bps \
    --caller "$SOURCE_PUBLIC_KEY" \
    --token_a "$tx" \
    --token_b "$ty" \
    --fee_bps 30 2>&1) || die "create_pool_with_fee_bps failed: $out"
  pool=$(invoke "$factory" get_pool --token_a "$tx" --token_b "$ty" | extract_contract_id)
  if [[ -z "$pool" ]]; then
    die "factory has no pool for the pair after create_pool_with_fee_bps: $out"
  fi

  invoke "$tx" mint --to "$SOURCE_PUBLIC_KEY" --amount "$amount" >/dev/null
  invoke "$ty" mint --to "$SOURCE_PUBLIC_KEY" --amount "$amount" >/dev/null
  deadline=$(( $(date +%s) + 300 ))
  invoke "$pool" add_liquidity \
    --provider "$SOURCE_PUBLIC_KEY" \
    --amount_a "$amount" \
    --amount_b "$amount" \
    --min_shares 0 \
    --deadline "$deadline" >/dev/null
  printf '%s' "$pool"
}

# pool_token POOL FIELD — the AMM pool's token_a or token_b. The factory
# stores a pair sorted by address, so a pool's token_a is not necessarily the
# token a flow passed as token_a, and add_liquidity's amount_a/amount_b follow
# the pool's order. Flows that deposit unequal amounts must mint by it.
pool_token() {
  invoke "$1" get_info | grep -Eo "\"$2\":\"C[A-Z2-7]{55}\"" | parse_address
}

# balance_of TOKEN ADDRESS — token balance as an integer.
balance_of() {
  invoke "$1" balance --id "$2" | parse_i128
}

# e2e_trap_errors — report the failing command when `set -e` aborts a flow,
# so a parse miss (e.g. an empty grep under pipefail) is not a silent exit.
e2e_trap_errors() {
  trap 'printf "[FAIL] %s: command failed at %s:%s: %s\n" "${CURRENT_FLOW:-e2e}" "$(basename "${BASH_SOURCE[0]:-e2e}")" "$LINENO" "$BASH_COMMAND" >&2' ERR
}

# e2e_standalone FLOW_FN — entrypoint for running one self-contained flow
# script directly: checks tooling, funds the source account, runs the flow,
# and exits non-zero on any failure.
e2e_standalone() {
  local fn="$1"
  require_cmd stellar
  generate_and_fund_source
  SOURCE_PUBLIC_KEY="$(stellar keys address "$SOURCE_ACCOUNT")"
  export NETWORK SOURCE_ACCOUNT SOURCE_PUBLIC_KEY
  e2e_trap_errors
  "$fn"
  printf '\n%s: %d passed, %d failed\n' "$fn" "$PASS_COUNT" "$FAIL_COUNT"
}
