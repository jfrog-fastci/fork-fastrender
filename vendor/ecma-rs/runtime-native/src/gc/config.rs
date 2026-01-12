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

impl HeapConfig {
  pub fn validate(&self) -> Result<(), &'static str> {
    if self.nursery_size_bytes == 0 {
      return Err("rt_gc_set_config: nursery_size_bytes must be non-zero");
    }
    if self.los_threshold_bytes == 0 {
      return Err("rt_gc_set_config: los_threshold_bytes must be non-zero");
    }
    if self.minor_gc_nursery_used_percent > 100 {
      return Err("rt_gc_set_config: minor_gc_nursery_used_percent must be in 0..=100");
    }
    if self.promote_after_minor_survivals == 0 {
      return Err("rt_gc_set_config: promote_after_minor_survivals must be >= 1");
    }
    Ok(())
  }

  pub fn to_rt(&self) -> crate::abi::RtGcConfig {
    // `RtGcConfig` has padding on 64-bit targets; ensure we don't leak uninitialized bytes across the
    // C ABI boundary when returning this value via `rt_gc_get_config`.
    let mut cfg: crate::abi::RtGcConfig = unsafe { core::mem::zeroed() };
    cfg.nursery_size_bytes = self.nursery_size_bytes;
    cfg.los_threshold_bytes = self.los_threshold_bytes;
    cfg.minor_gc_nursery_used_percent = self.minor_gc_nursery_used_percent;
    cfg.major_gc_old_bytes_threshold = self.major_gc_old_bytes_threshold;
    cfg.major_gc_old_blocks_threshold = self.major_gc_old_blocks_threshold;
    cfg.major_gc_external_bytes_threshold = self.major_gc_external_bytes_threshold;
    cfg.promote_after_minor_survivals = self.promote_after_minor_survivals;
    cfg
  }
}

impl HeapLimits {
  pub fn validate(&self) -> Result<(), &'static str> {
    if self.max_heap_bytes == 0 {
      return Err("rt_gc_set_limits: max_heap_bytes must be non-zero");
    }
    if self.max_total_bytes == 0 {
      return Err("rt_gc_set_limits: max_total_bytes must be non-zero");
    }
    if self.max_total_bytes < self.max_heap_bytes {
      return Err("rt_gc_set_limits: max_total_bytes must be >= max_heap_bytes");
    }
    Ok(())
  }

  pub fn to_rt(&self) -> crate::abi::RtGcLimits {
    crate::abi::RtGcLimits {
      max_heap_bytes: self.max_heap_bytes,
      max_total_bytes: self.max_total_bytes,
    }
  }
}

impl TryFrom<crate::abi::RtGcConfig> for HeapConfig {
  type Error = &'static str;

  fn try_from(cfg: crate::abi::RtGcConfig) -> Result<Self, Self::Error> {
    let config = Self {
      nursery_size_bytes: cfg.nursery_size_bytes,
      los_threshold_bytes: cfg.los_threshold_bytes,
      minor_gc_nursery_used_percent: cfg.minor_gc_nursery_used_percent,
      major_gc_old_bytes_threshold: cfg.major_gc_old_bytes_threshold,
      major_gc_old_blocks_threshold: cfg.major_gc_old_blocks_threshold,
      major_gc_external_bytes_threshold: cfg.major_gc_external_bytes_threshold,
      promote_after_minor_survivals: cfg.promote_after_minor_survivals,
    };
    config.validate()?;
    Ok(config)
  }
}

impl TryFrom<crate::abi::RtGcLimits> for HeapLimits {
  type Error = &'static str;

  fn try_from(limits: crate::abi::RtGcLimits) -> Result<Self, Self::Error> {
    let limits = Self {
      max_heap_bytes: limits.max_heap_bytes,
      max_total_bytes: limits.max_total_bytes,
    };
    limits.validate()?;
    Ok(limits)
  }
}

pub fn validate_config_and_limits(config: &HeapConfig, limits: &HeapLimits) -> Result<(), &'static str> {
  if config.nursery_size_bytes > limits.max_heap_bytes {
    return Err("invalid GC heap config: nursery_size_bytes must be <= max_heap_bytes");
  }
  Ok(())
}

pub fn apply_env_overrides(config: &mut HeapConfig, limits: &mut HeapLimits, apply_config: bool, apply_limits: bool) {
  fn parse_mb_env(name: &str) -> Option<usize> {
    let s = std::env::var(name).ok()?;
    let s_trimmed = s.trim();
    if s_trimmed.is_empty() {
      return None;
    }

    let mb = match s_trimmed.parse::<u64>() {
      Ok(mb) => mb,
      Err(_) => {
        eprintln!("runtime-native: ignoring {name}={s:?} (expected integer MiB)");
        return None;
      }
    };

    let bytes = match mb.checked_mul(1024 * 1024) {
      Some(bytes) => bytes,
      None => {
        eprintln!("runtime-native: ignoring {name}={s:?} (overflow)");
        return None;
      }
    };
    if bytes == 0 {
      eprintln!("runtime-native: ignoring {name}={s:?} (must be >= 1 MiB)");
      return None;
    }

    match usize::try_from(bytes) {
      Ok(bytes) => Some(bytes),
      Err(_) => {
        eprintln!("runtime-native: ignoring {name}={s:?} (overflow)");
        None
      }
    }
  }

  if apply_config {
    if let Some(bytes) = parse_mb_env("ECMA_RS_GC_NURSERY_MB") {
      config.nursery_size_bytes = bytes;
    }
  }

  if apply_limits {
    if let Some(bytes) = parse_mb_env("ECMA_RS_GC_MAX_HEAP_MB") {
      limits.max_heap_bytes = bytes;
    }
    if let Some(bytes) = parse_mb_env("ECMA_RS_GC_MAX_TOTAL_MB") {
      limits.max_total_bytes = bytes;
    }
  }
}
