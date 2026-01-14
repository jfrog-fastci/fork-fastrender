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
  # `git diff --raw` format:
  #   :<oldmode> <newmode> <oldsha> <newsha> <status>\t<path>
  git diff --raw --no-renames "${diff_args[@]}" | awk '
    /^:/ {
      oldmode = substr($1, 2);
      newmode = $2;
      oldsha = $3;
      newsha = $4;
      status = $5;
      path = $6;

      if ((status == "M" || status == "T") && oldmode != newmode && oldsha == newsha) {
        print path;
      }
    }
  '
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
