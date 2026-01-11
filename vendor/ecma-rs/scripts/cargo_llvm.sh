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

# Delegate to the standard cargo wrapper
exec bash "${REPO_ROOT}/../../scripts/cargo_agent.sh" "$@"
