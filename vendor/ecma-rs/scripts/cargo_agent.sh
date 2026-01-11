#!/usr/bin/env bash
set -euo pipefail

# Thin wrapper for running cargo commands against the nested `vendor/ecma-rs` workspace.
#
# In this repository, the high-throughput cargo wrapper lives at `<repo-root>/scripts/cargo_agent.sh`,
# but Cargo needs to run from `vendor/ecma-rs/` so it picks up:
# - `vendor/ecma-rs/Cargo.toml` (the correct workspace + `default-members`)
# - `vendor/ecma-rs/rust-toolchain.toml` (the pinned compiler version)
#
# Usage (from repo root):
#   bash vendor/ecma-rs/scripts/cargo_agent.sh test -p hir-js
#
# Usage (from vendor/ecma-rs):
#   bash scripts/cargo_agent.sh test -p hir-js
#   # (no `--workspace` => runs default members only)

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ECMA_RS_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
MONOREPO_ROOT="$(cd "${ECMA_RS_ROOT}/../.." && pwd)"

MONOREPO_WRAPPER="${MONOREPO_ROOT}/scripts/cargo_agent.sh"
if [[ ! -f "${MONOREPO_WRAPPER}" ]]; then
  echo "error: expected cargo wrapper at ${MONOREPO_WRAPPER}" >&2
  exit 1
fi

cd "${ECMA_RS_ROOT}"
exec bash "${MONOREPO_WRAPPER}" "$@"
