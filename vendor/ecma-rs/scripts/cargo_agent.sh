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

# Preserve the caller's working directory so we can normalize any relative
# `--manifest-path` args before switching into the nested workspace. This keeps
# invocations like:
#   bash vendor/ecma-rs/scripts/cargo_agent.sh check --manifest-path vendor/ecma-rs/Cargo.toml -p native-js
# working from the monorepo root.
CALLER_DIR="$(pwd)"

argv=("$@")
for ((i = 0; i < ${#argv[@]}; i++)); do
  # Stop once we reach the argument delimiter. Anything after `--` is forwarded
  # to rustc / the test harness / the executed binary, and should not be
  # rewritten.
  if [[ "${argv[$i]}" == "--" ]]; then
    break
  fi

  if [[ "${argv[$i]}" == "--manifest-path" ]]; then
    path="${argv[$((i + 1))]:-}"
    if [[ -n "${path}" && "${path}" != /* ]]; then
      argv[$((i + 1))]="${CALLER_DIR}/${path}"
    fi
  elif [[ "${argv[$i]}" == --manifest-path=* ]]; then
    path="${argv[$i]#--manifest-path=}"
    if [[ -n "${path}" && "${path}" != /* ]]; then
      argv[$i]="--manifest-path=${CALLER_DIR}/${path}"
    fi
  fi
done

# `runtime-native` relies on an FP-based stack walker for GC root enumeration.
# Ensure frame pointers are enabled so `bash vendor/ecma-rs/scripts/cargo_agent.sh check -p runtime-native`
# works out of the box (without requiring callers to manually set RUSTFLAGS or use `cargo_llvm.sh`).
if [[ "${RUSTFLAGS:-}" != *"force-frame-pointers=yes"* ]]; then
  if [[ -z "${RUSTFLAGS:-}" ]]; then
    export RUSTFLAGS="-C force-frame-pointers=yes"
  else
    export RUSTFLAGS="${RUSTFLAGS} -C force-frame-pointers=yes"
  fi
fi

cd "${ECMA_RS_ROOT}"
exec bash "${MONOREPO_WRAPPER}" "${argv[@]}"
