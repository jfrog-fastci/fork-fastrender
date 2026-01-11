#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ECMA_RS_ROOT="$(cd "${SCRIPT_DIR}/.." && pwd)"
SUPER_ROOT="$(cd "${ECMA_RS_ROOT}/../.." && pwd)"

cd "${ECMA_RS_ROOT}"

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
nm_syms_file="$(mktemp)"
trap 'rm -f "${nm_syms_file}"' EXIT
#
# `grep -q` can exit early, which can SIGPIPE the upstream producer and (with `pipefail`) turn a
# successful match into a failure. Write the nm output to a file once and query it.
"${nm_tool}" -g --defined-only "${staticlib}" | awk '{print $3}' | sort -u >"${nm_syms_file}"

required_symbols=(
  rt_alloc
  rt_alloc_pinned
  rt_alloc_array
  rt_gc_safepoint
  rt_write_barrier
  rt_gc_collect
  rt_gc_set_young_range
  rt_gc_get_young_range
  rt_string_concat
  rt_string_intern
  rt_parallel_spawn
  rt_parallel_join
  rt_parallel_for
  rt_async_spawn
  rt_async_poll
  rt_promise_new
  rt_promise_resolve
  rt_promise_reject
  rt_promise_then
  rt_coro_await
)

missing=0
for sym in "${required_symbols[@]}"; do
  if ! grep -qx "${sym}" "${nm_syms_file}"; then
    echo "missing runtime-native export: ${sym}" >&2
    missing=1
  fi
done
if [[ "${missing}" -ne 0 ]]; then
  exit 1
fi

if ! command -v clang-18 >/dev/null 2>&1; then
  echo "clang-18 not found in PATH" >&2
  exit 1
fi

out_dir="${ECMA_RS_ROOT}/target/runtime-native-abi-check"
mkdir -p "${out_dir}"
out_bin="${out_dir}/ffi_smoke"

echo "[runtime-native] Compiling C smoke test..."
clang-18 \
  -std=c11 \
  -Wall -Wextra -Werror \
  -I "${ECMA_RS_ROOT}/runtime-native/include" \
  -Wl,-T,"${ECMA_RS_ROOT}/runtime-native/stackmaps.ld" \
  "${ECMA_RS_ROOT}/runtime-native/examples/ffi_smoke.c" \
  "${staticlib}" \
  -o "${out_bin}" \
  -no-pie \
  -ldl -lpthread -lm

echo "[runtime-native] Running C smoke test..."
bash "${SUPER_ROOT}/scripts/run_limited.sh" --as 1G --cpu 60 -- "${out_bin}"

echo "[runtime-native] OK"
