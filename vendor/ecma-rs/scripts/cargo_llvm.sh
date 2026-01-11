#!/usr/bin/env bash
set -euo pipefail

# Wrapper for cargo commands that involve LLVM-heavy operations (native-js codegen).
#
# Usage:
#   scripts/cargo_llvm.sh build -p native-js --release
#   scripts/cargo_llvm.sh test -p native-js --lib
#
# This sets:
#   - Higher RAM limit (96GB default)
#   - LLVM environment variables (if LLVM 18 is installed)
#
# Use for:
#   - Building native-js (LLVM IR generation)
#   - Building runtime-native
#   - Running codegen tests
#   - Release builds with LTO

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"

# Preserve the caller's working directory so we can normalize any relative
# `--manifest-path` args before switching into the nested workspace.
CALLER_DIR="$(pwd)"

argv=("$@")
for ((i = 0; i < ${#argv[@]}; i++)); do
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
# Ensure we're running Cargo against the ecma-rs workspace, not the outer
# fastrender workspace (which excludes `vendor/ecma-rs`).
cd "${REPO_ROOT}"

# Higher RAM limit for LLVM operations (default 96GB, override with LLVM_LIMIT_AS)
export FASTR_CARGO_LIMIT_AS="${LLVM_LIMIT_AS:-96G}"

# Auto-detect LLVM 18 on Ubuntu if not already set
if [[ -z "${LLVM_SYS_180_PREFIX:-}" ]]; then
  if [[ -d /usr/lib/llvm-18 ]]; then
    export LLVM_SYS_180_PREFIX=/usr/lib/llvm-18
  fi
fi

# Add LLVM to PATH if available
if [[ -n "${LLVM_SYS_180_PREFIX:-}" && -d "${LLVM_SYS_180_PREFIX}/bin" ]]; then
  export PATH="${LLVM_SYS_180_PREFIX}/bin:${PATH}"
fi

# Delegate to the repo-local cargo wrapper (which runs Cargo from the ecma-rs workspace).
exec bash "${SCRIPT_DIR}/cargo_agent.sh" "${argv[@]}"
