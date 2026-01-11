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
#   - Frame pointers for native runtime + generated code stack walking
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
# Ensure we're running Cargo against the ecma-rs workspace, not the outer
# fastrender workspace (which excludes `vendor/ecma-rs`).
cd "${REPO_ROOT}"

# Higher RAM limit for LLVM operations (default 96GB, override with LLVM_LIMIT_AS)
export FASTR_CARGO_LIMIT_AS="${LLVM_LIMIT_AS:-96G}"

# Precise GC via LLVM statepoint stackmaps requires reliable stack walking. We currently
# enforce frame-pointer walking as the first milestone, so all Rust code that can run on
# GC-managed threads must be compiled with frame pointers enabled.
#
# Note: We deliberately set this in the LLVM wrapper script instead of globally for the
# whole workspace, since most crates don't need frame pointers.
if [[ "${RUSTFLAGS:-}" != *"force-frame-pointers=yes"* ]]; then
  if [[ -z "${RUSTFLAGS:-}" ]]; then
    export RUSTFLAGS="-C force-frame-pointers=yes"
  else
    export RUSTFLAGS="${RUSTFLAGS} -C force-frame-pointers=yes"
  fi
fi

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

# Fail fast with a clearer message than llvm-sys/inkwell when LLVM is missing.
if [[ -n "${LLVM_SYS_180_PREFIX:-}" && ! -d "${LLVM_SYS_180_PREFIX}" ]]; then
  echo "error: LLVM_SYS_180_PREFIX is set but does not exist: ${LLVM_SYS_180_PREFIX}" >&2
  echo "hint: install LLVM 18 and set LLVM_SYS_180_PREFIX=/usr/lib/llvm-18 (see vendor/ecma-rs/EXEC.plan.md)" >&2
  exit 1
fi

llvm_config=""
if command -v llvm-config-18 >/dev/null 2>&1; then
  llvm_config="llvm-config-18"
elif command -v llvm-config >/dev/null 2>&1; then
  llvm_config="llvm-config"
fi

if [[ -z "${llvm_config}" ]]; then
  echo "error: LLVM 18 not found (missing llvm-config-18/llvm-config on PATH)" >&2
  echo "hint: install llvm-18-dev and set LLVM_SYS_180_PREFIX=/usr/lib/llvm-18 (or run vendor/ecma-rs/scripts/check_system.sh)" >&2
  exit 1
fi

llvm_ver="$("${llvm_config}" --version 2>/dev/null || echo "")"
llvm_major="${llvm_ver%%.*}"
if ! [[ "${llvm_major}" =~ ^[0-9]+$ ]] || [[ "${llvm_major}" -ne 18 ]]; then
  echo "error: LLVM 18 is required (found ${llvm_config} version '${llvm_ver}')" >&2
  echo "hint: install llvm-18 + llvm-18-dev and ensure LLVM 18 tools are on PATH" >&2
  exit 1
fi

# Delegate to the repo-local cargo wrapper (which runs Cargo from the ecma-rs workspace).
exec bash "${SCRIPT_DIR}/cargo_agent.sh" "${argv[@]}"
