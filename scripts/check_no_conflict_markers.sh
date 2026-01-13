#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${repo_root}"

# Guardrail: never commit unresolved merge conflict markers into Rust sources.
#
# Why this exists:
# - `vendor/ecma-rs/vm-js/src/exec.rs` previously landed with `<<<<<<<` / `=======` / `>>>>>>>`
#   markers and broke compilation.
# - Some vendored fixtures (notably TypeScript's test suite) intentionally contain conflict-marker
#   strings; those directories must be excluded.

required_roots=(
  "vendor/ecma-rs/vm-js/src"
  "vendor/ecma-rs/parse-js/src"
  "vendor/ecma-rs/semantic-js/src"
  "vendor/ecma-rs/test262-semantic/src"
)

missing_roots=()
for root in "${required_roots[@]}"; do
  if [[ ! -d "${root}" ]]; then
    missing_roots+=("${root}")
  fi
done
if [[ "${#missing_roots[@]}" -ne 0 ]]; then
  echo "warning: expected source directories missing (conflict-marker scan may be incomplete):" >&2
  for root in "${missing_roots[@]}"; do
    echo "  - ${root}" >&2
  done
  echo "warning: if this is a legacy checkout with submodules, run: bash scripts/ci_init_ecma_rs_submodule.sh" >&2
fi

# Common conflict markers (including diff3-style `|||||||`).
#
# Keep the separator strict (`^=======$`) so we don't trip on long "======" rulers in docs/logs.
conflict_re='^(<<<<<<<|>>>>>>>|[|]{7}|=======[[:space:]]*$)'

have_rg=0
if command -v rg >/dev/null 2>&1; then
  have_rg=1
fi

matches=""
if command -v git >/dev/null 2>&1 && git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  # Prefer `git grep`: it is fast, and only scans tracked files (avoids traversing large fixture
  # trees like WPT).
  set +e
  matches="$(
    git grep -nE "${conflict_re}" -- \
      '*.rs' \
      ':!vendor/ecma-rs/parse-js/tests/TypeScript/**'
  )"
  status=$?
  set -e
  if [[ "${status}" -ne 0 && "${status}" -ne 1 ]]; then
    echo "error: git grep failed while scanning for conflict markers (exit ${status})" >&2
    exit "${status}"
  fi
elif [[ "${have_rg}" -eq 1 ]]; then
  # Fallback for environments without git (rare).
  set +e
  matches="$(
    rg -n \
      --glob '*.rs' \
      --glob '!vendor/ecma-rs/parse-js/tests/TypeScript/**' \
      "${conflict_re}" \
      src crates tests xtask vendor/ecma-rs/vm-js/src vendor/ecma-rs/parse-js/src vendor/ecma-rs/semantic-js/src vendor/ecma-rs/test262-semantic/src 2>/dev/null
  )"
  status=$?
  set -e
  if [[ "${status}" -ne 0 && "${status}" -ne 1 ]]; then
    echo "error: rg failed while scanning for conflict markers (exit ${status})" >&2
    exit "${status}"
  fi
else
  # Fallback for environments without ripgrep (e.g. minimal Windows shells).
  matches="$(
    grep -RInE \
      --include='*.rs' \
      "${conflict_re}" \
      src crates tests xtask vendor/ecma-rs/vm-js/src vendor/ecma-rs/parse-js/src vendor/ecma-rs/semantic-js/src vendor/ecma-rs/test262-semantic/src \
      2>/dev/null || true
  )"
fi

if [[ -n "${matches}" ]]; then
  echo "error: found unresolved merge conflict markers in Rust source files:" >&2
  echo "${matches}" >&2
  echo >&2
  echo "hint: resolve the merge conflicts and remove the marker lines (<<<<<<< / ======= / >>>>>>>)." >&2
  echo "note: conflict-marker fixtures under vendor/ecma-rs/parse-js/tests/TypeScript/** are allowed." >&2
  exit 1
fi
