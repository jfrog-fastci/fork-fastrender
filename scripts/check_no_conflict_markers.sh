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

append_matches() {
  local new_matches="$1"
  if [[ -z "${new_matches}" ]]; then
    return 0
  fi
  if [[ -z "${matches}" ]]; then
    matches="${new_matches}"
  else
    matches+=$'\n'"${new_matches}"
  fi
}

if command -v git >/dev/null 2>&1 && git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  # Prefer `git grep`: it is fast, and only scans tracked files (avoids traversing large fixture
  # trees like WPT).
  pathspecs=(
    ':(glob)src/**/*.rs'
    ':(glob)crates/**/*.rs'
    ':(glob)tests/**/*.rs'
    ':(glob)xtask/**/*.rs'
    ':(glob)benches/**/*.rs'
    ':(glob)fuzz/**/*.rs'
    ':(glob)tools/**/*.rs'

    # Required vendored sources.
    ':(glob)vendor/ecma-rs/vm-js/src/**/*.rs'
    ':(glob)vendor/ecma-rs/parse-js/src/**/*.rs'
    ':(glob)vendor/ecma-rs/semantic-js/src/**/*.rs'
    ':(glob)vendor/ecma-rs/test262-semantic/src/**/*.rs'

    # Known conflict-marker fixtures (allowed).
    ':!vendor/ecma-rs/parse-js/tests/TypeScript/**'
  )
  set +e
  git_matches="$(
    git grep -nE "${conflict_re}" -- "${pathspecs[@]}"
  )"
  status=$?
  set -e
  if [[ "${status}" -ne 0 && "${status}" -ne 1 ]]; then
    echo "error: git grep failed while scanning for conflict markers (exit ${status})" >&2
    exit "${status}"
  fi
  append_matches "${git_matches}"
elif [[ "${have_rg}" -eq 1 ]]; then
  # Fallback for environments without git (rare).
  set +e
  rg_matches="$(
    rg -n \
      --glob '*.rs' \
      --glob '!vendor/ecma-rs/parse-js/tests/TypeScript/**' \
      "${conflict_re}" \
      src crates tests xtask benches fuzz tools \
      vendor/ecma-rs/vm-js/src \
      vendor/ecma-rs/parse-js/src \
      vendor/ecma-rs/semantic-js/src \
      vendor/ecma-rs/test262-semantic/src \
      2>/dev/null
  )"
  status=$?
  set -e
  if [[ "${status}" -ne 0 && "${status}" -ne 1 ]]; then
    echo "error: rg failed while scanning for conflict markers (exit ${status})" >&2
    exit "${status}"
  fi
  append_matches "${rg_matches}"
else
  # Fallback for environments without ripgrep (e.g. minimal Windows shells).
  grep_matches="$(
    grep -RInE \
      --include='*.rs' \
      "${conflict_re}" \
      src crates tests xtask benches fuzz tools \
      vendor/ecma-rs/vm-js/src \
      vendor/ecma-rs/parse-js/src \
      vendor/ecma-rs/semantic-js/src \
      vendor/ecma-rs/test262-semantic/src \
      2>/dev/null || true
  )"
  append_matches "${grep_matches}"
fi

# In legacy checkouts `vendor/ecma-rs` may be a git submodule. The top-level `git grep` will not
# search inside submodules, so explicitly scan the ecma-rs repository if it exists as its own git
# work tree.
if command -v git >/dev/null 2>&1 && [[ -e vendor/ecma-rs/.git ]]; then
  set +e
  ecma_rs_matches="$(
    git -C vendor/ecma-rs grep -nE "${conflict_re}" -- \
      ':(glob)vm-js/src/**/*.rs' \
      ':(glob)parse-js/src/**/*.rs' \
      ':(glob)semantic-js/src/**/*.rs' \
      ':(glob)test262-semantic/src/**/*.rs' \
      ':!parse-js/tests/TypeScript/**'
  )"
  status=$?
  set -e
  if [[ "${status}" -ne 0 && "${status}" -ne 1 ]]; then
    echo "error: git grep failed while scanning vendor/ecma-rs submodule for conflict markers (exit ${status})" >&2
    exit "${status}"
  fi
  if [[ -n "${ecma_rs_matches}" ]]; then
    # Prefix submodule-local paths so errors point at the superproject file locations.
    ecma_rs_matches="$(printf '%s\n' "${ecma_rs_matches}" | sed 's|^|vendor/ecma-rs/|')"
    append_matches "${ecma_rs_matches}"
  fi
fi

if [[ -n "${matches}" ]]; then
  matches="$(printf '%s\n' "${matches}" | sort -u)"
fi

if [[ -n "${matches}" ]]; then
  echo "error: found unresolved merge conflict markers in Rust source files:" >&2
  echo "${matches}" >&2
  echo >&2
  echo "hint: resolve the merge conflicts and remove the marker lines (<<<<<<< / ======= / >>>>>>>)." >&2
  echo "note: conflict-marker fixtures under vendor/ecma-rs/parse-js/tests/TypeScript/** are allowed." >&2
  exit 1
fi
