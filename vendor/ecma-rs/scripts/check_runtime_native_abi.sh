#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ECMA_RS_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
SUPER_ROOT="$(cd "${ECMA_RS_ROOT}/../.." && pwd)"

cd "${ECMA_RS_ROOT}"

pick_cmd() {
  for c in "$@"; do
    if command -v "${c}" >/dev/null 2>&1; then
      echo "${c}"
      return 0
    fi
  done
  return 1
}

CLANG="${ECMA_RS_NATIVE_CLANG:-}"
if [[ -z "${CLANG}" ]]; then
  if ! CLANG="$(pick_cmd clang-18 clang)"; then
    echo "clang not found in PATH (expected clang-18 or clang)" >&2
    exit 1
  fi
fi
if ! command -v "${CLANG}" >/dev/null 2>&1; then
  echo "clang not found in PATH: ${CLANG}" >&2
  exit 1
fi

runtime_native_include_dir="${ECMA_RS_ROOT}/runtime-native/include"
runtime_native_header="${runtime_native_include_dir}/runtime_native.h"

derive_required_symbols() {
  local out_file="$1"
  shift

  # Derive the required export set from the (preprocessed) stable ABI header.
  #
  # This intentionally avoids a hand-maintained symbol list (which tends to
  # drift). To simulate a failure locally, temporarily rename one `rt_*`
  # declaration in `runtime_native.h` or remove `#[no_mangle]` from the
  # corresponding Rust export, then re-run this script: it should print the
  # missing symbol name and exit non-zero.
  #
  # Notes:
  # - We preprocess the header so `#ifdef RUNTIME_NATIVE_GC_STATS` /
  #   `RUNTIME_NATIVE_GC_DEBUG` blocks are included/excluded based on the passed
  #   `-D...` args (default: off).
  # - We operate on preprocessed output (comments stripped) to avoid matching
  #   symbols in comments.
  local preprocessed_header
  preprocessed_header="$(mktemp)"
  tmp_files+=("${preprocessed_header}")

  "${CLANG}" \
    -E -P \
    -x c -std=c11 \
    -I "${runtime_native_include_dir}" \
    "$@" \
    "${runtime_native_header}" \
    >"${preprocessed_header}"

  {
    grep -oE '(^|[^A-Za-z0-9_])rt_[A-Za-z0-9_]+[[:space:]]*\(' "${preprocessed_header}" \
      | sed -E 's/^[^A-Za-z0-9_]*//; s/[[:space:]]*\($//' \
      || true

    # Variable exports (not matched by the function regex).
    echo "RT_GC_EPOCH"
    echo "RT_THREAD"
  } | sort -u >"${out_file}"

  if [[ ! -s "${out_file}" ]]; then
    echo "failed to derive required runtime-native exports from ${runtime_native_header}" >&2
    exit 1
  fi
  if ! grep -q '^rt_' "${out_file}"; then
    echo "failed to extract any rt_* exports from ${runtime_native_header}" >&2
    exit 1
  fi
}

nm_defined_exports() {
  local staticlib="$1"
  local out_file="$2"

  "${nm_tool}" -g --defined-only "${staticlib}" \
    | awk 'NF >= 3 { print $3 }' \
    | sort -u \
    >"${out_file}"
}

check_missing_exports() {
  local label="$1"
  local required_file="$2"
  local exported_file="$3"

  local missing_file
  missing_file="$(mktemp)"
  tmp_files+=("${missing_file}")

  comm -23 "${required_file}" "${exported_file}" >"${missing_file}"
  if [[ -s "${missing_file}" ]]; then
    echo "[runtime-native] Missing exports (${label}):" >&2
    cat "${missing_file}" >&2
    exit 1
  fi
}

echo "[runtime-native] Building staticlib..."
bash scripts/cargo_llvm.sh build -p runtime-native

lib_debug="${ECMA_RS_ROOT}/target/debug/libruntime_native.a"
lib_release="${ECMA_RS_ROOT}/target/release/libruntime_native.a"
if [[ -f "${lib_debug}" ]]; then
  staticlib="${lib_debug}"
elif [[ -f "${lib_release}" ]]; then
  staticlib="${lib_release}"
else
  echo "runtime-native static library not found in target/{debug,release}" >&2
  echo "expected one of:" >&2
  echo "  ${lib_debug}" >&2
  echo "  ${lib_release}" >&2
  exit 1
fi

nm_tool=""
if command -v llvm-nm-18 >/dev/null 2>&1; then
  nm_tool="llvm-nm-18"
elif command -v llvm-nm >/dev/null 2>&1; then
  nm_tool="llvm-nm"
elif command -v nm >/dev/null 2>&1; then
  nm_tool="nm"
else
  echo "neither llvm-nm nor nm found in PATH" >&2
  exit 1
fi

echo "[runtime-native] Verifying exported symbols via ${nm_tool}..."
tmp_files=()
cleanup() {
  if [[ "${#tmp_files[@]}" -gt 0 ]]; then
    rm -f "${tmp_files[@]}"
  fi
}
trap cleanup EXIT

nm_syms_file="$(mktemp)"
tmp_files+=("${nm_syms_file}")
#
# `grep -q` can exit early, which can SIGPIPE the upstream producer and (with `pipefail`) turn a
# successful match into a failure. Write the nm output to a file once and query it.
nm_defined_exports "${staticlib}" "${nm_syms_file}"

required_syms_file="$(mktemp)"
tmp_files+=("${required_syms_file}")
derive_required_symbols "${required_syms_file}" -U RUNTIME_NATIVE_GC_STATS -U RUNTIME_NATIVE_GC_DEBUG
check_missing_exports "default build" "${required_syms_file}" "${nm_syms_file}"

out_dir="${ECMA_RS_ROOT}/target/runtime-native-abi-check"
mkdir -p "${out_dir}"
out_bin="${out_dir}/ffi_smoke"

stackmaps_ld="${ECMA_RS_ROOT}/runtime-native/link/stackmaps.ld"
if [[ ! -f "${stackmaps_ld}" ]]; then
  stackmaps_ld="${ECMA_RS_ROOT}/runtime-native/stackmaps.ld"
fi

echo "[runtime-native] Compiling C smoke test..."
"${CLANG}" \
  -std=c11 \
  -Wall -Wextra -Werror \
  -I "${runtime_native_include_dir}" \
  -Wl,-T,"${stackmaps_ld}" \
  "${ECMA_RS_ROOT}/runtime-native/examples/ffi_smoke.c" \
  "${staticlib}" \
  -o "${out_bin}" \
  -no-pie \
  -ldl -lpthread -lm

# Optional (bonus): also verify feature-gated GC exports.
#
# Run with:
#   RUNTIME_NATIVE_ABI_CHECK_GC_FEATURES=1 bash scripts/check_runtime_native_abi.sh
if [[ "${RUNTIME_NATIVE_ABI_CHECK_GC_FEATURES:-0}" == "1" ]]; then
  echo "[runtime-native] Building staticlib with --features gc_stats,gc_debug..."
  bash scripts/cargo_llvm.sh build -p runtime-native --features gc_stats,gc_debug

  # Re-scan exports for the feature build (same output path, different Cargo feature set).
  nm_syms_gc_file="$(mktemp)"
  tmp_files+=("${nm_syms_gc_file}")
  nm_defined_exports "${staticlib}" "${nm_syms_gc_file}"

  required_syms_gc_file="$(mktemp)"
  tmp_files+=("${required_syms_gc_file}")
  derive_required_symbols "${required_syms_gc_file}" \
    -DRUNTIME_NATIVE_GC_STATS \
    -DRUNTIME_NATIVE_GC_DEBUG
  check_missing_exports "gc_stats+gc_debug build" "${required_syms_gc_file}" "${nm_syms_gc_file}"
fi

echo "[runtime-native] Running C smoke test..."
# The runtime-native bump allocator reserves a large virtual-address arena by
# default. Under the tight `--as 1G` limit used by this smoke test, that default
# can fail even though the smoke test itself allocates very little.
#
# Use a small bump arena unless the caller explicitly configured one.
# Also cap the worker pool size: on large CI hosts, spawning one worker per CPU
# can consume a large amount of virtual address space (thread stacks), which can
# exceed the tight `--as 1G` limit used by this smoke test.
RUNTIME_NATIVE_BUMP_ARENA_SIZE="${RUNTIME_NATIVE_BUMP_ARENA_SIZE:-256M}" \
ECMA_RS_RUNTIME_NATIVE_THREADS="${ECMA_RS_RUNTIME_NATIVE_THREADS:-4}" \
  bash "${SUPER_ROOT}/scripts/run_limited.sh" --as 1G --cpu 60 -- "${out_bin}"

echo "[runtime-native] OK"
