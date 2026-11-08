#!/usr/bin/env bash
# scripts/check_test_policy.sh — enforce the clean-server test strategy.
#
# See system-documents/TEST_STRATEGY.md for the rules this script enforces.
# In one line: it rejects placeholders in production code, tests that assert
# nothing, ignored tests without a documented reason, orphan test files not
# assigned to a tier, and empty test files.
#
# Usage:
#   scripts/check_test_policy.sh --tier 1|2|3      # gate for that tier
#   scripts/check_test_policy.sh --explain         # print tier assignments and allowlist
#
# Exit code is 0 on success, 1 on any policy violation.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
ALLOWLIST="$SCRIPT_DIR/.test-policy-allowlist"

# ---------------------------------------------------------------------------
# Tier membership. Every file in tests/ MUST appear in exactly one tier.
# When you add a new test file, add it here. The orphan check (P5) enforces
# that new tests get a tier assignment before landing.
# ---------------------------------------------------------------------------

TIER1_FILES=(
  # T1 (fast) is unit tests via cargo test --lib. No integration files here.
)

TIER2_FILES=(
  bridge_contract_test.rs
  bridge_compliance_test.rs
  canvas_stubs_test.rs
  ui_stubs_test.rs
  wasm_alignment_test.rs
  jobs_persistence_test.rs
  jobs_bridge_test.rs
  jwt_refresh_rotation_test.rs
  reset_token_bridge_test.rs
  string_split_test.rs
  page_guard_redirect_test.rs
  http_bridge_defaults_test.rs
  req_body_bytes_bridge_test.rs
  crypto_sha256_bytes_bridge_test.rs
  dev_snapshot_bridge_test.rs
  json_v2_bridge_test.rs
  json_conformance.rs
  startup_state_test.rs
  schedule_cron_bridge_test.rs
  request_state_bridge_test.rs
  db_transaction_bridge_test.rs
  async_task_bridge_test.rs
)

TIER3_FILES=(
  host_functions_test.rs
  server_smoke_test.rs
)

# ---------------------------------------------------------------------------
# Argument parsing
# ---------------------------------------------------------------------------

TIER=""
EXPLAIN=0

while [[ $# -gt 0 ]]; do
  case "$1" in
    --tier) TIER="$2"; shift 2 ;;
    --explain) EXPLAIN=1; shift ;;
    -h|--help)
      grep '^#' "$0" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *) echo "Unknown arg: $1" >&2; exit 2 ;;
  esac
done

if [[ "$EXPLAIN" -eq 1 ]]; then
  echo "Tier 1 files (T1 is cargo test --lib; no integration files):"
  printf '  %s\n' "${TIER1_FILES[@]:-<none>}"
  echo
  echo "Tier 2 files:"
  printf '  %s\n' "${TIER2_FILES[@]}"
  echo
  echo "Tier 3 files:"
  printf '  %s\n' "${TIER3_FILES[@]}"
  echo
  echo "Allowlist ($ALLOWLIST):"
  if [[ -f "$ALLOWLIST" ]]; then
    sed 's/^/  /' "$ALLOWLIST"
  else
    echo "  <missing>"
  fi
  exit 0
fi

if [[ -z "$TIER" ]]; then
  echo "check_test_policy: --tier {1|2|3} is required" >&2
  exit 2
fi

cd "$REPO_ROOT"

# ---------------------------------------------------------------------------
# Allowlist loader
# ---------------------------------------------------------------------------

# Populate an associative-array-lite via a plain lookup function so we work on
# bash 3 (the default on macOS) as well as bash 4+.
allowlist_contains() {
  local rule="$1" location="$2"
  [[ -f "$ALLOWLIST" ]] || return 1
  grep -qE "^${rule}[[:space:]]+${location}([[:space:]]|$)" "$ALLOWLIST"
}

VIOLATIONS=()

record() {
  VIOLATIONS+=("$1")
}

# ---------------------------------------------------------------------------
# P1 — no todo!() / unimplemented!() / panic!("not implemented") in src/
# ---------------------------------------------------------------------------

check_p1() {
  local pattern
  # Combine three patterns; grep -nE for line numbers and extended regex.
  pattern='todo!\(|unimplemented!\(|panic!\("not implemented'
  while IFS=: read -r file line _rest; do
    [[ -z "$file" ]] && continue
    # Skip comment-only matches (comments are handled by P2).
    if allowlist_contains "P1" "$file:$line"; then continue; fi
    record "P1 $file:$line — todo!/unimplemented!/panic!(\"not implemented\") in production code"
  done < <(grep -rnE "$pattern" src host-bridge/src --include='*.rs' 2>/dev/null || true)
}

# ---------------------------------------------------------------------------
# P2 — no // TODO / // FIXME in src/ outside allowlist
# ---------------------------------------------------------------------------

check_p2() {
  # Restrict to actual comment lines; ignore matches inside string literals by
  # requiring the marker to be preceded by `//`.
  while IFS=: read -r file line _rest; do
    [[ -z "$file" ]] && continue
    if allowlist_contains "P2" "$file:$line"; then continue; fi
    record "P2 $file:$line — // TODO or // FIXME in production code (convert to TASKS.md entry)"
  done < <(grep -rnE '//[[:space:]]*(TODO|FIXME)\b' src host-bridge/src --include='*.rs' 2>/dev/null || true)
}

# ---------------------------------------------------------------------------
# P3 — every #[test] / #[tokio::test] fn must contain at least one
#      assert!/assert_eq!/assert_ne!/panic!/expect(/unwrap( call
# ---------------------------------------------------------------------------
#
# We implement this with an awk pass per file. It's O(bytes) and covers both
# integration and inline unit tests.

check_p3() {
  local files
  # Every .rs file under tests/ (integration) and src/ + host-bridge/src/
  # (inline unit tests inside `mod tests`).
  files=$(find tests src host-bridge/src -type f -name '*.rs' 2>/dev/null || true)
  [[ -z "$files" ]] && return 0

  local file
  while read -r file; do
    [[ -z "$file" ]] && continue
    awk -v F="$file" '
      # Detect a test attribute on the current line; remember it and its line
      # number so we can attribute the following fn to it.
      /^[[:space:]]*#\[(tokio::)?test\]/ { pending_test=1; test_line=NR; next }
      # Start of an fn immediately after a test attribute.
      pending_test && /^[[:space:]]*(async[[:space:]]+)?fn[[:space:]]+[A-Za-z0-9_]+/ {
        in_test=1; pending_test=0; brace_depth=0; found_assert=0; fn_line=NR
        # Count braces on the fn signature line itself (single-line fns exist).
        line=$0
        for (i=1;i<=length(line);i++) {
          c=substr(line,i,1)
          if (c=="{") brace_depth++
          else if (c=="}") brace_depth--
        }
        # If the fn opened and closed on the same line without asserts, flag it.
        if (in_test && brace_depth==0) {
          print "P3 " F ":" fn_line " — test function has no assert!/expect/unwrap"
          in_test=0
        }
        next
      }
      # A #[test] attribute not immediately followed by an fn is invalid Rust,
      # so we clear the pending flag on any other non-blank non-attribute line.
      pending_test && !/^[[:space:]]*$/ && !/^[[:space:]]*#\[/ { pending_test=0 }
      # While inside a test fn, track braces and look for assertion tokens.
      in_test {
        line=$0
        for (i=1;i<=length(line);i++) {
          c=substr(line,i,1)
          if (c=="{") brace_depth++
          else if (c=="}") brace_depth--
        }
        if (line ~ /assert!|assert_eq!|assert_ne!|assert_matches!|panic!\(|\.expect\(|\.unwrap\(|\.unwrap_or_else\(|\.unwrap_err\(/) {
          found_assert=1
        }
        if (brace_depth<=0) {
          if (!found_assert) {
            print "P3 " F ":" fn_line " — test function has no assert!/expect/unwrap"
          }
          in_test=0
        }
      }
    ' "$file"
  done <<< "$files" | while IFS= read -r line; do
    [[ -z "$line" ]] && continue
    # Strip the "P3 " prefix, look up the allowlist as file:line.
    local loc="${line#P3 }"
    loc="${loc%% —*}"
    if allowlist_contains "P3" "$loc"; then continue; fi
    record "$line"
  done
}

# ---------------------------------------------------------------------------
# P4 — no #[ignore] without an "// allowlisted: <reason>" marker
# ---------------------------------------------------------------------------

check_p4() {
  while IFS=: read -r file line content; do
    [[ -z "$file" ]] && continue
    if [[ "$content" == *"allowlisted:"* ]]; then continue; fi
    if allowlist_contains "P4" "$file:$line"; then continue; fi
    record "P4 $file:$line — #[ignore] without '// allowlisted: <reason>' on same line"
  done < <(grep -rnE '#\[ignore(\(|\])' tests src host-bridge/src --include='*.rs' 2>/dev/null || true)
}

# ---------------------------------------------------------------------------
# P5 — every file in tests/*.rs must be assigned to a tier
# ---------------------------------------------------------------------------

check_p5() {
  local assigned=()
  # ${arr[@]:+"${arr[@]}"} preserves the "empty array is OK" semantics under set -u.
  assigned+=(${TIER1_FILES[@]:+"${TIER1_FILES[@]}"})
  assigned+=(${TIER2_FILES[@]:+"${TIER2_FILES[@]}"})
  assigned+=(${TIER3_FILES[@]:+"${TIER3_FILES[@]}"})

  local f base found
  while IFS= read -r f; do
    [[ -z "$f" ]] && continue
    base="$(basename "$f")"
    found=0
    for a in "${assigned[@]}"; do
      if [[ "$a" == "$base" ]]; then found=1; break; fi
    done
    if [[ "$found" -eq 0 ]]; then
      if allowlist_contains "P5" "tests/$base"; then continue; fi
      record "P5 tests/$base — orphan test file; add it to a TIER*_FILES list in scripts/check_test_policy.sh"
    fi
  done < <(find tests -maxdepth 1 -type f -name '*.rs' 2>/dev/null || true)
}

# ---------------------------------------------------------------------------
# P6 — every tests/*.rs must contain at least one #[test]/#[tokio::test] fn
# ---------------------------------------------------------------------------

check_p6() {
  local f base
  while IFS= read -r f; do
    [[ -z "$f" ]] && continue
    base="$(basename "$f")"
    if grep -qE '^[[:space:]]*#\[(tokio::)?test(\(.*\))?\]' "$f" 2>/dev/null; then continue; fi
    if allowlist_contains "P6" "tests/$base"; then continue; fi
    record "P6 tests/$base — no #[test] or #[tokio::test] function found"
  done < <(find tests -maxdepth 1 -type f -name '*.rs' 2>/dev/null || true)
}

# ---------------------------------------------------------------------------
# Run checks appropriate for the tier
# ---------------------------------------------------------------------------

case "$TIER" in
  1)
    # Fast lane: only the checks that don't require walking every file heavily.
    check_p1
    check_p2
    check_p5
    check_p6
    ;;
  2|3)
    check_p1
    check_p2
    check_p3
    check_p4
    check_p5
    check_p6
    ;;
  *)
    echo "check_test_policy: unknown tier '$TIER' (expected 1|2|3)" >&2
    exit 2
    ;;
esac

# ---------------------------------------------------------------------------
# Report
# ---------------------------------------------------------------------------

if [[ "${#VIOLATIONS[@]}" -eq 0 ]]; then
  echo "check_test_policy: PASSED (tier $TIER)"
  exit 0
fi

echo "check_test_policy: FAILED (tier $TIER) — ${#VIOLATIONS[@]} violation(s):" >&2
for v in "${VIOLATIONS[@]}"; do
  echo "  $v" >&2
done
echo >&2
echo "See system-documents/TEST_STRATEGY.md § 5 for policy details." >&2
echo "To allowlist an entry: add a line to scripts/.test-policy-allowlist" >&2
exit 1
