# GC configuration (runtime-native)

`runtime-native` uses a single **process-global** GC heap for the exported fast allocators
(`rt_alloc`, `rt_alloc_array`, `rt_alloc_pinned`) and collectors (`rt_gc_collect*`).

The heap is initialized lazily on first use (typically during `rt_thread_init`, the first
allocation, or the first explicit `rt_gc_collect*` call).

## Configuration API (preferred for tests/embedders)

Before the heap is initialized, embedders (and tests) can tune GC policy/sizing via the stable C
ABI:

- `rt_gc_set_config(&RtGcConfig)` sets collection policy / thresholds.
- `rt_gc_set_limits(&RtGcLimits)` sets hard caps (`max_heap_bytes`, `max_total_bytes`).

These setters must be called **before** the heap is initialized. If called after initialization,
they return `false` and have no effect.

For debugging, `rt_gc_get_config` / `rt_gc_get_limits` snapshot the current effective settings
(pending defaults before initialization, or the live heap config after initialization).

## Environment variable overrides (defaults only)

If `rt_gc_set_config` / `rt_gc_set_limits` were **not** called explicitly, heap initialization reads
these environment variables **once** (integer MiB values):

- `ECMA_RS_GC_NURSERY_MB`
- `ECMA_RS_GC_MAX_HEAP_MB`
- `ECMA_RS_GC_MAX_TOTAL_MB`

Env overrides apply only to defaults (they do not override an embedder-provided config set via the
ABI).

## Example (Rust test)

```rust
use runtime_native::abi::{RtGcConfig, RtGcLimits};
use runtime_native::{rt_gc_set_config, rt_gc_set_limits, rt_thread_init};

let cfg = RtGcConfig {
  nursery_size_bytes: 64 * 1024,
  los_threshold_bytes: 8 * 1024,
  minor_gc_nursery_used_percent: 1,
  major_gc_old_bytes_threshold: usize::MAX,
  major_gc_old_blocks_threshold: usize::MAX,
  major_gc_external_bytes_threshold: usize::MAX,
  promote_after_minor_survivals: 1,
};

let limits = RtGcLimits {
  max_heap_bytes: 8 * 1024 * 1024,
  max_total_bytes: 16 * 1024 * 1024,
};

assert!(rt_gc_set_config(&cfg));
assert!(rt_gc_set_limits(&limits));

// First use initializes the heap.
rt_thread_init(0);
```
