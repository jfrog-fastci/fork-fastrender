#!/usr/bin/env bash
set -euo pipefail

# Runs only the tests listed in typescript_conformance_failures.txt.
# Accepts extra arguments passed to the conformance runner after `--`.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
ECMA_RS_ROOT="$(cd "${ROOT}/.." && pwd)"

# Use the repo's constrained cargo wrapper so:
# - multi-agent hosts don't stampede the linker/rustc
# - we build against the ecma-rs workspace + pinned toolchain even when invoked from outside the
#   workspace directory.
exec bash "${ECMA_RS_ROOT}/scripts/cargo_agent.sh" run -p parse-js --features conformance-runner --bin conformance_runner -- \
  --failures "${ROOT}/typescript_conformance_failures.txt" "$@"
