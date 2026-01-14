#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd -- "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${repo_root}"

# Revert *filemode-only* diffs (100755 ↔ 100644 with identical blob content) without touching
# legitimate content changes.
#
# This is meant as a safe cleanup when your filesystem / tooling flips the executable bit and Git
# shows a large number of `old mode/new mode` diffs.

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

# 1) Fix staged mode-only changes by resetting the index to HEAD.
#
# Important: this can *create* new unstaged mode-only diffs (the common case when you staged a
# mode-only change while the worktree already matched the index). We therefore recompute the
# unstaged list after resetting the index.
staged="$(mode_only_paths --cached)"
if [[ -n "${staged}" ]]; then
  echo "reverting staged filemode-only changes..." >&2
  while IFS= read -r path; do
    [[ -z "${path}" ]] && continue
    git restore --staged --source=HEAD -- "${path}"
  done <<<"${staged}"
fi

# 2) Fix unstaged mode-only changes by resetting the worktree to the index.
#
# Recompute after any index updates above so we catch the "staged only" case too.
unstaged="$(mode_only_paths)"
if [[ -n "${unstaged}" ]]; then
  echo "reverting unstaged filemode-only changes..." >&2
  while IFS= read -r path; do
    [[ -z "${path}" ]] && continue
    git restore --worktree -- "${path}"
  done <<<"${unstaged}"
fi

if [[ -z "${staged}" && -z "${unstaged}" ]]; then
  echo "no filemode-only changes found" >&2
fi
