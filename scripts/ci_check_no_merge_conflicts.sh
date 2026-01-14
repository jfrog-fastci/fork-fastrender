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
# - tests/wpt*/{tests,resources,expected}/**: vendored Web Platform Tests corpora (large, not
#   project source). Note that we still scan curated manifests like `tests/wpt_dom/expectations.toml`
#   so configuration files can't accidentally land with conflict markers.
# - vendor/ecma-rs/test262*/data/**: vendored JS corpora (massive, not project source).
#
# Implementation note:
# - When scanning the repository (default), prefer `git grep` so we only scan tracked files. This
#   avoids false positives from build outputs and avoids missing tracked-but-ignored files (gitignore
#   does not apply to tracked files, but tools like ripgrep still skip them).
# - When scanning an arbitrary directory via `--path`, fall back to `rg` or a small Python walker.

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
use_git=0
if [[ "${scan_root}" == "." ]] && command -v git >/dev/null 2>&1; then
  if git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
    use_git=1
  fi
fi

if [[ "${use_git}" -eq 1 ]]; then
  pathspecs=(
    "${scan_root}"
    ':!vendor/ecma-rs/parse-js/tests/TypeScript/**'
    ':!specs/**'
    ':!tests/wpt/_import_testdata/**'
    ':!tests/wpt/_offline_validator_testdata/**'
    ':!tests/wpt/expected/**'
    ':!tests/wpt/tests/**'
    ':!tests/wpt_dom/resources/**'
    ':!tests/wpt_dom/tests/**'
    ':!vendor/ecma-rs/test262/data/**'
    ':!vendor/ecma-rs/test262-semantic/data/**'
  )

  matches="$(
    git grep -n -I \
      -e '^<<<<<<< ' \
      -e '^||||||| ' \
      -e '^=======[[:space:]]*$' \
      -e '^>>>>>>> ' \
      -- "${pathspecs[@]}"
  )"
  status=$?
elif [[ "${have_rg}" -eq 1 ]]; then
  matches="$(
    rg -n --hidden --no-messages \
      -e '^<<<<<<< ' \
      -e '^[|]{7} ' \
      -e '^=======[[:space:]]*$' \
      -e '^>>>>>>> ' \
      --glob '!.git/**' \
      --glob '!vendor/ecma-rs/parse-js/tests/TypeScript/**' \
      --glob '!specs/**' \
      --glob '!tests/wpt/_import_testdata/**' \
      --glob '!tests/wpt/_offline_validator_testdata/**' \
      --glob '!tests/wpt/expected/**' \
      --glob '!tests/wpt/tests/**' \
      --glob '!tests/wpt_dom/resources/**' \
      --glob '!tests/wpt_dom/tests/**' \
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
    "${py}" - <<'PY' "${scan_root}" "${repo_root}"
 import os
 import re
 import sys
 from pathlib import Path
 
 root = Path(sys.argv[1]).resolve()
 repo_root = Path(sys.argv[2]).resolve()
 
 marker_re = re.compile(r"^(<<<<<<<\s|\|\|\|\|\|\|\|\s|=======\s*$|>>>>>>>\s)")
 
 # Note: when scanning `--path <dir>`, we still want to apply the same exclusions as the main
 # repo-wide scan (spec submodules, large WPT/test262 corpora, etc). Use absolute paths so the
 # exclusions remain correct regardless of `--path` root.
 excluded_dirs = [
     repo_root / ".git",
     repo_root / "target",
     repo_root / "target_pages",
     repo_root / "fetches",
     repo_root / "tmp",
     repo_root / "vendor/ecma-rs/parse-js/tests/TypeScript",
     repo_root / "specs",
     repo_root / "tests/wpt/_import_testdata",
     repo_root / "tests/wpt/_offline_validator_testdata",
     repo_root / "tests/wpt/expected",
     repo_root / "tests/wpt/tests",
     repo_root / "tests/wpt_dom/resources",
     repo_root / "tests/wpt_dom/tests",
     repo_root / "vendor/ecma-rs/test262/data",
     repo_root / "vendor/ecma-rs/test262-semantic/data",
 ]
 excluded_dirs = [p.resolve() for p in excluded_dirs]
 
 def should_skip_abs(path: Path) -> bool:
     path = path.resolve()
     for ex in excluded_dirs:
         try:
             path.relative_to(ex)
             return True
         except ValueError:
             pass
     return False
 
 # If the scan root itself is an excluded directory, skip the scan entirely.
 if should_skip_abs(root):
     sys.exit(0)
 
 for dirpath, dirnames, filenames in os.walk(root):
     # Prune excluded directories.
     for d in list(dirnames):
         if should_skip_abs(Path(dirpath) / d):
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
if [[ "${scan_root}" == "." && -e vendor/ecma-rs/.git ]]; then
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
    echo "hint: resolve the conflict and delete the <<<<<<< / ||||||| / ======= / >>>>>>> lines before committing." >&2
    exit 1
  fi

  if [[ "${ecma_rs_status}" -ne 1 ]]; then
    echo "error: failed to scan vendor/ecma-rs for merge-conflict markers (git grep exit ${ecma_rs_status})" >&2
    exit "${ecma_rs_status}"
  fi
fi
