#!/usr/bin/env bash
set -euo pipefail

# Thin wrapper for running cargo commands against the nested `vendor/ecma-rs` workspace.
#
# In this repository, the high-throughput cargo wrapper lives at `./scripts/cargo_agent.sh` (repo
# root), but Cargo needs to run from `vendor/ecma-rs/` so it picks up `vendor/ecma-rs/Cargo.toml`.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

cd "${REPO_ROOT}"
exec bash "${REPO_ROOT}/../../scripts/cargo_agent.sh" "$@"

