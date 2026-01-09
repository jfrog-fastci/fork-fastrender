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

git submodule update --init engines/ecma-rs

