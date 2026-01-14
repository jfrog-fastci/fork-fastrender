#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${repo_root}"

# Detect filemode-only diffs (e.g. accidental 100755 ↔ 100644 flips) in either the working tree or
# the index.
#
# We intentionally only flag cases where the blob hash is unchanged (content identical) but the mode
# changed. Legitimate mode changes that accompany content changes are left alone.

mode_only_paths() {
  local diff_args=("$@")
  # We use `--raw -z` so paths with spaces are handled correctly, and `--no-abbrev` so blob ids are
  # full-length (needed when comparing to `git hash-object`, which outputs full object ids).
  #
  # `git diff --raw -z` format (no renames):
  #   :<oldmode> <newmode> <oldsha> <newsha> <status>\0<path>\0
  #
  # For *unstaged* diffs (index ↔ worktree), Git may print the "newsha" as all zeros instead of
  # hashing the working-tree file content. When that happens, we compute the worktree blob id via
  # `git hash-object -- <path>` and only treat it as mode-only if it matches <oldsha> from the index.
  local header path
  local -a needs_hash_paths=()
  local -a needs_hash_oldshas=()

  while IFS= read -r -d '' header; do
    IFS= read -r -d '' path || break

    header="${header#:}"
    local oldmode newmode oldsha newsha status
    read -r oldmode newmode oldsha newsha status <<<"${header}"

    # Mode-only change: same blob hash, different mode. `M` (modified) is typical, but `T` (type
    # change) is also possible; handle both.
    [[ "${status}" != "M" && "${status}" != "T" ]] && continue
    [[ "${oldmode}" == "${newmode}" ]] && continue

    # Staged diffs (`--cached`) have a real <newsha> so the straight comparison works.
    if [[ "${oldsha}" == "${newsha}" ]]; then
      printf '%s\n' "${path}"
      continue
    fi

    # Unstaged diffs may report an all-zero <newsha>; hash the worktree file to verify content.
    if [[ "${newsha}" =~ ^0+$ ]]; then
      needs_hash_paths+=("${path}")
      needs_hash_oldshas+=("${oldsha}")
    fi
  done < <(git diff --raw -z --no-renames --no-abbrev "${diff_args[@]}")

  if ((${#needs_hash_paths[@]} == 0)); then
    return 0
  fi

  local -a worktree_shas=()
  mapfile -t worktree_shas < <(git hash-object -- "${needs_hash_paths[@]}")
  local i
  for i in "${!needs_hash_paths[@]}"; do
    if [[ "${worktree_shas[${i}]}" == "${needs_hash_oldshas[${i}]}" ]]; then
      printf '%s\n' "${needs_hash_paths[${i}]}"
    fi
  done
}

unstaged="$(mode_only_paths)"
staged="$(mode_only_paths --cached)"

if [[ -z "${unstaged}" && -z "${staged}" ]]; then
  exit 0
fi

echo "error: found filemode-only git diffs (content unchanged; mode differs, e.g. 100755 ↔ 100644):" >&2
if [[ -n "${staged}" ]]; then
  echo >&2
  echo "staged:" >&2
  echo "${staged}" >&2
fi
if [[ -n "${unstaged}" ]]; then
  echo >&2
  echo "unstaged:" >&2
  echo "${unstaged}" >&2
fi

cat >&2 <<'EOF'

hint: if these are accidental, revert them before committing/pushing:

  # Safe auto-fix (reverts mode-only changes without touching content changes):
  bash scripts/revert_filemode_only_changes.sh

or manually:

  # Unstaged mode-only changes:
  git restore --worktree <path>

  # Staged mode-only changes:
  git restore --staged --source=HEAD <path>

or to wipe *all* accidental changes in this checkout (destructive):

  git restore --source=HEAD --staged --worktree .

note: if your filesystem frequently flips executable bits, consider setting:

  git config core.filemode false

(This is a local-only setting; do not commit it.)
EOF

exit 1
