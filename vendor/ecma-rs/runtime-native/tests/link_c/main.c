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

extern void rt_thread_init(uint32_t kind);
extern void rt_thread_deinit(void);
extern void rt_gc_collect(void);
extern void rt_gc_safepoint(void);

int main(void) {
  // `rt_gc_safepoint` is intended to be called from threads registered with the runtime (and
  // asserts in debug builds if the current thread is not registered).
  rt_thread_init(3);
  rt_gc_safepoint();
  rt_gc_collect();
  rt_thread_deinit();
  return 0;
}
