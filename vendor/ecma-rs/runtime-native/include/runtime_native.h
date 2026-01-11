#ifndef ECMA_RS_RUNTIME_NATIVE_H
#define ECMA_RS_RUNTIME_NATIVE_H

// Minimal stable C ABI surface for runtime-native.
//
// This header is intended for code generators / native glue code. Keep it small:
// only entrypoints that are part of the compiler/runtime ABI contract should live here.

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

// GC entrypoints (see docs/write_barrier.md)
void rt_gc_safepoint(void);
void rt_write_barrier(uint8_t* obj, uint8_t* slot);
void rt_gc_collect(void);

#ifdef __cplusplus
} // extern "C"
#endif

#endif // ECMA_RS_RUNTIME_NATIVE_H
