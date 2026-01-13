#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${repo_root}"

# Guardrail: fail fast if tracked Rust sources contain unresolved git merge-conflict markers.
#
# We intentionally scope this check to Rust/TOML sources (instead of scanning the entire repository)
# to avoid false positives from vendored test corpora that may intentionally embed these strings.

if ! command -v git >/dev/null 2>&1; then
  echo "error: git not available; cannot scan for merge-conflict markers" >&2
  exit 2
fi

if ! git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
  echo "error: not inside a git work tree; cannot scan for merge-conflict markers" >&2
  exit 2
fi

set +e
matches="$(
  git grep -n -I \
    -e '^<<<<<<< ' \
    -e '^||||||| ' \
    -e '^=======[[:space:]]*$' \
    -e '^>>>>>>> ' \
    -- \
    '*.rs' \
    '*.toml'
)"
status=$?
set -e

if [[ "${status}" -eq 0 ]]; then
  echo "error: found unresolved git merge-conflict markers in tracked Rust sources:" >&2
  echo "${matches}" >&2
  echo >&2
  echo "hint: resolve the conflict and delete the <<<<<<< / ======= / >>>>>>> lines before committing." >&2
  exit 1
fi

if [[ "${status}" -ne 1 ]]; then
  echo "error: failed to scan repository for merge-conflict markers (git grep exit ${status})" >&2
  exit "${status}"
fi

# Legacy compatibility: older checkouts used `vendor/ecma-rs` as a git submodule. The top-level
# `git grep` does not search inside submodules, so explicitly scan the ecma-rs work tree when it
# exists as its own git repository.
if [[ -e vendor/ecma-rs/.git ]]; then
  set +e
  ecma_rs_matches="$(
    git -C vendor/ecma-rs grep -n -I \
      -e '^<<<<<<< ' \
      -e '^||||||| ' \
      -e '^=======[[:space:]]*$' \
      -e '^>>>>>>> ' \
      -- \
      '*.rs' \
      '*.toml'
  )"
  ecma_rs_status=$?
  set -e

  if [[ "${ecma_rs_status}" -eq 0 ]]; then
    ecma_rs_matches="$(printf '%s\n' "${ecma_rs_matches}" | sed 's|^|vendor/ecma-rs/|')"
    echo "error: found unresolved git merge-conflict markers in vendor/ecma-rs:" >&2
    echo "${ecma_rs_matches}" >&2
    echo >&2
    echo "hint: resolve the conflict and delete the <<<<<<< / ======= / >>>>>>> lines before committing." >&2
    exit 1
  fi

  if [[ "${ecma_rs_status}" -ne 1 ]]; then
    echo "error: failed to scan vendor/ecma-rs for merge-conflict markers (git grep exit ${ecma_rs_status})" >&2
    exit "${ecma_rs_status}"
  fi
fi
