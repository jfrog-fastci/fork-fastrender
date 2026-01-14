#!/usr/bin/env bash
set -euo pipefail

# Focused vm-js integration test subset for generator/`yield` correctness.
#
# These tests execute arbitrary JavaScript and can hang forever (`while(true){}`), so every
# invocation is wrapped in a hard wall-clock timeout (`timeout -k`).
#
# This script is intended to be:
# - A standard developer command
# - A lightweight CI smoke suite to catch regressions early

repo_root="$(cd -- "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${repo_root}"

if [[ ! -f vendor/ecma-rs/Cargo.toml ]]; then
  echo "error: missing vendor/ecma-rs checkout (expected vendor/ecma-rs/Cargo.toml)" >&2
  echo "hint: run: git submodule update --init vendor/ecma-rs" >&2
  exit 1
fi

timeout_bin="timeout"
if ! command -v "${timeout_bin}" >/dev/null 2>&1; then
  # macOS users often install GNU coreutils as `gtimeout`.
  if command -v gtimeout >/dev/null 2>&1; then
    timeout_bin="gtimeout"
  else
    echo "error: missing 'timeout' (GNU coreutils)." >&2
    echo "hint (macOS): brew install coreutils  # then retry with gtimeout in PATH" >&2
    exit 1
  fi
fi

# Keep defaults aligned with `instructions/js_engine.md`.
timeout_secs="${VM_JS_TEST_TIMEOUT_SECS:-600}"
timeout_kill_secs="${VM_JS_TEST_TIMEOUT_KILL_SECS:-10}"

run_vm_js_test() {
  local test_target="$1"
  echo "==> vm-js integration test: ${test_target}"
  "${timeout_bin}" -k "${timeout_kill_secs}" "${timeout_secs}" \
    bash vendor/ecma-rs/scripts/cargo_agent.sh test -p vm-js --test "${test_target}"
}

# Required generator/yield regression set:
run_vm_js_test generators_yield_operators
run_vm_js_test generators_delete_yield
run_vm_js_test generators_destructuring_assignment_yield

# Keep the test target name stable: CI relies on this exact integration test name.
run_vm_js_test generators_binary_ops_yield
