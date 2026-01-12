#!/usr/bin/env bash
set -euo pipefail

# Linker wrapper used for Rust builds on Linux.
#
# We prefer `mold` when available because the default GNU `ld` can be extremely slow when linking
# large debug binaries (and our crate is large). However, we don't want to hard-require `mold`,
# since some environments may not have it installed.

if [[ "${FASTR_USE_MOLD:-1}" != "0" ]] && command -v ld.mold >/dev/null 2>&1; then
  exec clang -fuse-ld=mold "$@"
else
  exec clang "$@"
fi

