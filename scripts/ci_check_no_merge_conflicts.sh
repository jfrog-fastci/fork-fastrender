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
