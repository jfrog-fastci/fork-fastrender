// Minimal C linker smoke test for the runtime-native staticlib.
//
// This file intentionally avoids including `runtime_native.h`: we want to ensure
// a plain C toolchain can link against the `runtime-native` staticlib and call
// exported symbols. Minimal declarations are duplicated here on purpose.
//
// The runtime's GC/safepoint entrypoints require that the calling thread is
// registered (mirrors the contract for any thread that may run compiled code).
// Keep this test tiny: it exists only to ensure the archive can be linked and
// its symbols invoked from a plain C program.
#include <stdint.h>
#include <stddef.h>
#include <stdbool.h>

typedef struct RtGcConfig {
  size_t nursery_size_bytes;
  size_t los_threshold_bytes;
  uint8_t minor_gc_nursery_used_percent;
  size_t major_gc_old_bytes_threshold;
  size_t major_gc_old_blocks_threshold;
  size_t major_gc_external_bytes_threshold;
  uint8_t promote_after_minor_survivals;
} RtGcConfig;

typedef struct RtGcLimits {
  size_t max_heap_bytes;
  size_t max_total_bytes;
} RtGcLimits;

extern void rt_thread_init(uint32_t kind);
extern void rt_thread_deinit(void);
extern void rt_gc_collect(void);
extern void rt_gc_safepoint(void);
extern bool rt_gc_set_config(const RtGcConfig* cfg);
extern bool rt_gc_set_limits(const RtGcLimits* limits);

int main(void) {
  // Ensure the process-global heap config API is callable from a plain C program.
  RtGcConfig cfg = {
    .nursery_size_bytes = 1024 * 1024,
    .los_threshold_bytes = 8 * 1024,
    .minor_gc_nursery_used_percent = 80,
    .major_gc_old_bytes_threshold = (size_t)-1,
    .major_gc_old_blocks_threshold = (size_t)-1,
    .major_gc_external_bytes_threshold = (size_t)-1,
    .promote_after_minor_survivals = 1,
  };
  RtGcLimits limits = {
    .max_heap_bytes = 256 * 1024 * 1024,
    .max_total_bytes = 512 * 1024 * 1024,
  };
  if (!rt_gc_set_config(&cfg)) {
    return 100;
  }
  if (!rt_gc_set_limits(&limits)) {
    return 101;
  }

  // `rt_gc_safepoint` is intended to be called from threads registered with the runtime (and
  // asserts in debug builds if the current thread is not registered).
  rt_thread_init(3);
  rt_gc_safepoint();
  rt_gc_collect();
  rt_thread_deinit();
  return 0;
}
