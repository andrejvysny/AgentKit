#!/usr/bin/env bash
# Ad hoc sign the macOS microVM helper with the hardened runtime and its one
# entitlement, com.apple.security.virtualization (ADR 0040, section 3).
#
#   scripts/sign-helper.sh [HELPER]
#
# HELPER defaults to rust/target/debug/agentkit-sandbox-vmhelper. The binary
# is signed in place; build it first (from rust/):
#   cargo build -p agentkit-sandbox-microvm --bin agentkit-sandbox-vmhelper
# A later cargo build or test of the crate relinks that file and drops the
# signature, so sign a copy outside target/ for tests (tests/real_vm.rs).
# A Developer ID signature for distribution is not decided yet (ADR 0040).
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CRATE="$(cd "$HERE/.." && pwd)"
ENTITLEMENTS="$CRATE/entitlements/vmhelper.plist"
HELPER="${1:-$CRATE/../../target/debug/agentkit-sandbox-vmhelper}"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "sign-helper: codesign needs macOS" >&2
  exit 2
fi
if [[ ! -f "$HELPER" ]]; then
  echo "sign-helper: no helper at $HELPER; build it first" >&2
  exit 2
fi

codesign --force --options runtime --entitlements "$ENTITLEMENTS" -s - "$HELPER"
codesign --display --entitlements - "$HELPER"
