#!/usr/bin/env bash
set -euo pipefail

# Repo-local wrapper for the top-level `scripts/run_limited.sh`.
#
# This exists so vendored ecma-rs docs can reference `vendor/ecma-rs/scripts/run_limited.sh`
# when invoked from the repository root.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

exec bash "${REPO_ROOT}/../../scripts/run_limited.sh" "$@"

