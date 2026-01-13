#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${repo_root}"

# Guardrail: fail fast if the work tree contains unresolved git merge-conflict markers.
#
# This check is intentionally repo-wide (not just Rust) to catch accidental conflict markers early,
# but we exclude known large fixture/vendor corpora that intentionally contain these strings.
#
# Exclusions (documented to avoid "mysterious" CI failures):
# - vendor/ecma-rs/parse-js/tests/TypeScript/**: TypeScript's own test suite includes conflict marker
#   trivia fixtures (e.g. `formatConflictMarker1.ts`).
# - specs/**: spec submodules use `=======` as a delimiter in Bikeshed sources.
# - tests/wpt{,_dom,_suites}/**: vendored Web Platform Tests corpora (massive, not project source).
# - vendor/ecma-rs/test262*/data/**: vendored JS corpora (massive, not project source).
#
# Note: we respect .gitignore by default to avoid scanning build outputs (target/, tmp/, ...). The
# goal is to block committing conflict markers in source, not to lint generated artifacts.

usage() {
  cat <<'EOF'
Usage: bash scripts/ci_check_no_merge_conflicts.sh [--path <dir>]

Scans for unresolved git merge-conflict markers:
  ^<<<<<<<␠
  ^|||||||␠  (diff3-style "common ancestors" marker)
  ^=======  (exact line; optional trailing whitespace)
  ^>>>>>>>␠

This runs in CI and is safe to run locally:
  bash scripts/ci_check_no_merge_conflicts.sh
EOF
}

scan_root="."
if [[ "${#}" -ne 0 ]]; then
  case "${1}" in
    -h|--help)
      usage
      exit 0
      ;;
    --path)
      if [[ "${#}" -ne 2 ]]; then
        echo "error: --path requires an argument" >&2
        usage >&2
        exit 2
      fi
      scan_root="${2}"
      ;;
    *)
      echo "error: unknown argument: ${1}" >&2
      usage >&2
      exit 2
      ;;
  esac
fi

have_rg=0
if command -v rg >/dev/null 2>&1; then
  have_rg=1
fi

set +e
if [[ "${have_rg}" -eq 1 ]]; then
  matches="$(
    rg -n --hidden --no-messages \
      -e '^<<<<<<< ' \
      -e '^[|]{7} ' \
      -e '^=======[[:space:]]*$' \
      -e '^>>>>>>> ' \
      --glob '!.git/**' \
      --glob '!vendor/ecma-rs/parse-js/tests/TypeScript/**' \
      --glob '!specs/**' \
      --glob '!tests/wpt/**' \
      --glob '!tests/wpt_dom/**' \
      --glob '!tests/wpt_suites/**' \
      --glob '!vendor/ecma-rs/test262/data/**' \
      --glob '!vendor/ecma-rs/test262-semantic/data/**' \
      -- "${scan_root}"
  )"
  status=$?
else
  if ! command -v python3 >/dev/null 2>&1 && ! command -v python >/dev/null 2>&1; then
    echo "error: neither ripgrep (rg) nor python is available; cannot scan for merge-conflict markers" >&2
    exit 2
  fi

  py="python3"
  if ! command -v python3 >/dev/null 2>&1; then
    py="python"
  fi

  matches="$(
    "${py}" - <<'PY' "${scan_root}"
import os
import re
import sys
from pathlib import Path

root = Path(sys.argv[1]).resolve()

marker_re = re.compile(r"^(<<<<<<<\s|\|\|\|\|\|\|\|\s|=======\s*$|>>>>>>>\s)")

skip_prefixes = [
    ".git",
    "target",
    "target_pages",
    "fetches",
    "tmp",
    "vendor/ecma-rs/parse-js/tests/TypeScript",
    "specs",
    "tests/wpt",
    "tests/wpt_dom",
    "tests/wpt_suites",
    "vendor/ecma-rs/test262/data",
    "vendor/ecma-rs/test262-semantic/data",
]

def should_skip_dir(rel_posix: str) -> bool:
    for prefix in skip_prefixes:
        if rel_posix == prefix or rel_posix.startswith(prefix + "/"):
            return True
    return False

for dirpath, dirnames, filenames in os.walk(root):
    rel = Path(dirpath).resolve().relative_to(root).as_posix()
    if rel == ".":
        rel = ""

    # Prune excluded directories.
    pruned = []
    for d in list(dirnames):
        sub_rel = f"{rel}/{d}" if rel else d
        if should_skip_dir(sub_rel):
            pruned.append(d)
    for d in pruned:
        dirnames.remove(d)

    for name in filenames:
        path = Path(dirpath) / name
        try:
            with path.open("rb") as f:
                data = f.read()
        except OSError:
            continue

        # Skip binary-ish files.
        if b"\0" in data:
            continue

        try:
            text = data.decode("utf-8", errors="replace")
        except Exception:
            continue

        for idx, line in enumerate(text.splitlines(), start=1):
            if marker_re.match(line):
                rel_path = path.resolve().as_posix()
                print(f"{rel_path}:{idx}:{line}")
PY
  )"
  status=$?
fi
set -e

if [[ "${status}" -eq 0 && -n "${matches}" ]]; then
  echo "error: found unresolved git merge-conflict markers:" >&2
  echo "${matches}" >&2
  echo >&2
  echo "hint: resolve the conflict and delete the <<<<<<< / ||||||| / ======= / >>>>>>> lines before committing." >&2
  exit 1
fi

if [[ "${status}" -ne 0 && "${status}" -ne 1 ]]; then
  echo "error: failed to scan repository for merge-conflict markers (exit ${status})" >&2
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
