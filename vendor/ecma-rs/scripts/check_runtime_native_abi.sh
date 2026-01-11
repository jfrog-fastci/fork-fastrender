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
  # Thread registration / shape table.
  rt_thread_init
  rt_thread_deinit
  rt_register_current_thread
  rt_unregister_current_thread
  rt_register_thread
  rt_unregister_thread
  rt_thread_register
  rt_thread_unregister
  rt_thread_set_parked
  rt_thread_attach
  rt_thread_detach
  rt_alloc
  rt_alloc_pinned
  rt_alloc_array
  rt_register_shape_table
  RT_GC_EPOCH
  rt_gc_poll
  rt_gc_safepoint
  rt_gc_safepoint_relocate_h
  rt_gc_safepoint_slow
  rt_keep_alive_gc_ref
  rt_write_barrier
  rt_write_barrier_range
  rt_gc_collect
  rt_backing_store_external_bytes
  rt_root_push
  rt_root_pop
  rt_gc_register_root_slot
  rt_gc_unregister_root_slot
  rt_gc_pin
  rt_gc_unpin
  rt_gc_set_young_range
  rt_gc_get_young_range
  rt_weak_add
  rt_weak_get
  rt_weak_remove
  rt_string_concat
  rt_string_intern
  rt_string_pin_interned
  rt_parallel_spawn
  rt_parallel_join
  rt_parallel_for
  rt_spawn_blocking

  # Native async ABI (PromiseHeader-based).
  rt_promise_init
  rt_promise_fulfill
  rt_promise_reject

  rt_async_spawn
  rt_async_spawn_deferred
  rt_async_poll
  rt_async_set_strict_await_yields

  # Legacy async ABI (temporary; will be removed once codegen migrates).
  rt_promise_new_legacy
  rt_promise_resolve_legacy
  rt_promise_resolve_into_legacy
  rt_promise_resolve_promise_legacy
  rt_promise_resolve_thenable_legacy
  rt_promise_reject_legacy
  rt_promise_then_legacy
  rt_async_spawn_legacy
  rt_async_spawn_deferred_legacy
  rt_async_poll_legacy
  rt_async_sleep_legacy
  rt_coro_await_legacy
  rt_coro_await_value_legacy
  rt_queue_microtask
  rt_set_timeout
  rt_set_interval
  rt_clear_timer
  rt_io_register
  rt_io_update
  rt_io_unregister
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

stackmaps_ld="${ECMA_RS_ROOT}/runtime-native/link/stackmaps.ld"
if [[ ! -f "${stackmaps_ld}" ]]; then
  stackmaps_ld="${ECMA_RS_ROOT}/runtime-native/stackmaps.ld"
fi

echo "[runtime-native] Compiling C smoke test..."
clang-18 \
  -std=c11 \
  -Wall -Wextra -Werror \
  -I "${ECMA_RS_ROOT}/runtime-native/include" \
  -Wl,-T,"${stackmaps_ld}" \
  "${ECMA_RS_ROOT}/runtime-native/examples/ffi_smoke.c" \
  "${staticlib}" \
  -o "${out_bin}" \
  -no-pie \
  -ldl -lpthread -lm

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
