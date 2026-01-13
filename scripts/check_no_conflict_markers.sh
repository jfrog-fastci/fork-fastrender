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

search_roots=(
  "vendor/ecma-rs/vm-js/src"
  "vendor/ecma-rs/parse-js/src"
  "vendor/ecma-rs/semantic-js/src"
  "vendor/ecma-rs/test262-semantic/src"
)

missing_roots=()
for root in "${search_roots[@]}"; do
  if [[ ! -d "${root}" ]]; then
    missing_roots+=("${root}")
  fi
done
if [[ "${#missing_roots[@]}" -ne 0 ]]; then
  echo "error: expected source directories missing (cannot scan for conflict markers):" >&2
  for root in "${missing_roots[@]}"; do
    echo "  - ${root}" >&2
  done
  exit 1
fi

# Common conflict marker prefixes (including diff3-style `|||||||`).
conflict_re='^(<{7}|={7}|>{7}|\|{7})'

have_rg=0
if command -v rg >/dev/null 2>&1; then
  have_rg=1
fi

matches=""
if [[ "${have_rg}" -eq 1 ]]; then
  # Ripgrep is fast and supports multiple globs; keep the scan scoped to Rust sources.
  #
  # Note: `vendor/ecma-rs/parse-js/tests/TypeScript/**` contains legitimate conflict-marker
  # fixtures (e.g. `formatConflictMarker1.ts`); explicitly exclude it so future broadening of
  # this check can't accidentally break on those fixtures.
  set +e
  matches="$(
    rg -n \
      --glob '*.rs' \
      --glob '!vendor/ecma-rs/parse-js/tests/TypeScript/**' \
      "${conflict_re}" \
      "${search_roots[@]}"
  )"
  status=$?
  set -e
  if [[ "${status}" -ne 0 && "${status}" -ne 1 ]]; then
    echo "error: rg failed while scanning for conflict markers (exit ${status})" >&2
    exit "${status}"
  fi
else
  # Fallback for environments without ripgrep (e.g. minimal Windows shells).
  # This scan is intentionally restricted to Rust sources, so the TypeScript fixture tree is not
  # traversed even if present.
  matches="$(
    grep -RInE \
      --include='*.rs' \
      "${conflict_re}" \
      "${search_roots[@]}" \
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
