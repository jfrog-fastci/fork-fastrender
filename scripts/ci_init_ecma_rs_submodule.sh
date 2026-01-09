#!/usr/bin/env bash
set -euo pipefail

# CI should always fetch submodules over HTTPS.
#
# Some developer environments configure:
#   url.git@github.com:.insteadof https://github.com/
# which rewrites submodule URLs to SSH and breaks checkouts in CI environments that do not have
# GitHub SSH keys configured.
#
# Unset the common rewrite keys defensively (no-op if they are not present).
git config --global --unset-all url.git@github.com:.insteadof 2>/dev/null || true
git config --global --unset-all url.ssh://git@github.com/.insteadof 2>/dev/null || true

git config --global core.longpaths true

# Ensure any `.gitmodules` URL changes are reflected in `.git/config`.
git submodule sync -- engines/ecma-rs

# A submodule gitlink can briefly reference a commit that isn't yet visible on GitHub
# (e.g. the ecma-rs push is still propagating), which manifests as:
#   remote error: upload-pack: not our ref <sha>
#
# Retry a few times with exponential backoff to eliminate flaky CI failures while still
# failing fast for genuinely-missing commits.
max_attempts="${FASTR_ECMA_RS_SUBMODULE_ATTEMPTS:-5}"
sleep_s=2
attempt=1
while true; do
  if git submodule update --init engines/ecma-rs; then
    exit 0
  fi

  if [[ "${attempt}" -ge "${max_attempts}" ]]; then
    echo "Failed to init engines/ecma-rs submodule after ${max_attempts} attempts." >&2
    exit 1
  fi

  echo "Retrying ecma-rs submodule init (${attempt}/${max_attempts}) in ${sleep_s}s..." >&2
  sleep "${sleep_s}"
  attempt=$((attempt + 1))
  sleep_s=$((sleep_s * 2))
done
