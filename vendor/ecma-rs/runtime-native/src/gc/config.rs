/// GC heap configuration.
///
/// This is intentionally a "policy + sizing" struct: it controls which spaces exist, when GC is
/// triggered, and how objects are promoted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeapConfig {
  /// Size of the nursery, in bytes.
  pub nursery_size_bytes: usize,
  /// Allocation size threshold above which objects go to the large object space (LOS).
  pub los_threshold_bytes: usize,

  /// Trigger a minor collection when nursery usage exceeds this percentage (`0..=100`).
  pub minor_gc_nursery_used_percent: u8,

  /// Trigger a major collection when old-generation live bytes exceed this threshold.
  pub major_gc_old_bytes_threshold: usize,
  /// Trigger a major collection when the old generation owns more than this number of Immix
  /// blocks.
  pub major_gc_old_blocks_threshold: usize,

  /// Trigger a major collection when externally allocated (non-GC) memory exceeds this threshold.
  ///
  /// This is intended to account for memory owned by non-moving allocations such as `ArrayBuffer`
  /// backing stores.
  pub major_gc_external_bytes_threshold: usize,

  /// Promotion policy: promote an object to the old generation after it has survived at least this
  /// many minor collections.
  ///
  /// A value of `1` means "promote on first survival".
  pub promote_after_minor_survivals: u8,
}

impl Default for HeapConfig {
  fn default() -> Self {
    Self {
      nursery_size_bytes: crate::nursery::DEFAULT_NURSERY_SIZE_BYTES,
      los_threshold_bytes: 8 * 1024,
      minor_gc_nursery_used_percent: 80,
      major_gc_old_bytes_threshold: 64 * 1024 * 1024,
      major_gc_old_blocks_threshold: 2048,
      // Default: start full collections once external memory reaches 64 MiB.
      //
      // Rationale: backing stores can be large and are not counted in the moving heap size.
      // Without an external-memory trigger, a program can run out of memory without the GC getting
      // a chance to finalize unreachable `ArrayBuffer` headers.
      major_gc_external_bytes_threshold: 64 * 1024 * 1024,
      promote_after_minor_survivals: 1,
    }
  }
}

/// Hard heap limits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeapLimits {
  /// Hard cap on heap memory usage, in bytes.
  pub max_heap_bytes: usize,

  /// Hard cap on total memory usage including external (non-GC) allocations, in bytes.
  ///
  /// This cap is enforced using [`GcHeap::external_bytes()`] in addition to GC heap accounting.
  /// It exists to prevent unbounded growth from `ArrayBuffer`/`TypedArray` backing stores.
  pub max_total_bytes: usize,
}

impl Default for HeapLimits {
  fn default() -> Self {
    Self {
      max_heap_bytes: 256 * 1024 * 1024,
      // Default: allow external allocations to grow beyond the GC heap cap, but still keep an
      // overall upper bound to avoid process OOM.
      max_total_bytes: 512 * 1024 * 1024,
    }
  }
}
