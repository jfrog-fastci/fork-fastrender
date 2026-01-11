#!/usr/bin/env bash
set -euo pipefail

# Thin wrapper for running cargo commands against the nested `vendor/ecma-rs` workspace.
#
# In this repository, the high-throughput cargo wrapper lives at `<repo-root>/scripts/cargo_agent.sh`,
# but Cargo needs to run from `vendor/ecma-rs/` so it picks up:
# - `vendor/ecma-rs/Cargo.toml` (the correct workspace)
# - `vendor/ecma-rs/rust-toolchain.toml` (the pinned compiler version)
#
# Usage (from repo root):
#   bash vendor/ecma-rs/scripts/cargo_agent.sh test -p hir-js
#
# Usage (from vendor/ecma-rs):
#   bash scripts/cargo_agent.sh test -p hir-js

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ECMA_RS_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

cd "${ECMA_RS_ROOT}"
exec bash "${ECMA_RS_ROOT}/../../scripts/cargo_agent.sh" "$@"
