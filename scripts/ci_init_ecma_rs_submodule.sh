#!/usr/bin/env bash
set -euo pipefail

# CI helper: ensure the vendored `vendor/ecma-rs` tree is available.
#
# Historical note:
# - Older revisions of this repo used `vendor/ecma-rs` as a git submodule.
# - Today the source is vendored directly into the repository, but several CI workflows
#   still call this script as a shared “ensure ecma-rs is present” hook.
#
# This script intentionally does *not* initialize the heavyweight nested corpora
# (`vendor/ecma-rs/test262*/data`, `vendor/ecma-rs/parse-js/tests/TypeScript`, …). Workflows that
# need those should `git submodule update --init <path>` explicitly so the default CI path stays
# lightweight.

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "${repo_root}"

if [[ -f vendor/ecma-rs/Cargo.toml ]]; then
  exit 0
fi

# Backwards compatibility for older checkouts where `vendor/ecma-rs` is a submodule.
if grep -qE '^[[:space:]]*path[[:space:]]*=[[:space:]]*vendor/ecma-rs[[:space:]]*$' .gitmodules 2>/dev/null; then
  git submodule update --init vendor/ecma-rs
  exit 0
fi

echo "::error::Missing vendor/ecma-rs checkout (expected vendor/ecma-rs/Cargo.toml)." >&2
echo "If your clone is missing vendored files, ensure you have a full checkout of the repository." >&2
exit 1

