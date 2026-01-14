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

      # Mode-only change: same blob hash, different mode. `M` (modified) is typical, but `T` (type
      # change) is also possible; handle both.
      if ((status == "M" || status == "T") && oldmode != newmode && oldsha == newsha) {
        print path;
      }
    }
  '
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
