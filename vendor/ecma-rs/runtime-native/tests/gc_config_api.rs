use std::process::Command;
use std::sync::Once;
use runtime_native::abi::RtGcConfig;
use runtime_native::abi::RtGcLimits;
use runtime_native::abi::RtShapeDescriptor;
use runtime_native::abi::RtShapeId;
use runtime_native::rt_alloc;
use runtime_native::rt_gc_collect;
use runtime_native::rt_gc_get_config;
use runtime_native::rt_gc_get_limits;
use runtime_native::rt_gc_get_young_range;
use runtime_native::rt_gc_set_config;
use runtime_native::rt_gc_set_limits;
use runtime_native::rt_thread_deinit;
use runtime_native::rt_thread_init;
use runtime_native::shape_table;
use runtime_native::test_util::TestRuntimeGuard;

static SHAPE_TABLE_ONCE: Once = Once::new();
static EMPTY_PTR_OFFSETS: [u32; 0] = [];
static SHAPES: [RtShapeDescriptor; 1] = [RtShapeDescriptor {
  size: 256,
  align: 16,
  flags: 0,
  ptr_offsets: EMPTY_PTR_OFFSETS.as_ptr(),
  ptr_offsets_len: 0,
  reserved: 0,
}];

fn ensure_shape_table() {
  SHAPE_TABLE_ONCE.call_once(|| unsafe {
    shape_table::rt_register_shape_table(SHAPES.as_ptr(), SHAPES.len());
  });
}

fn read_config() -> RtGcConfig {
  let mut out = core::mem::MaybeUninit::<RtGcConfig>::uninit();
  let ok = unsafe { rt_gc_get_config(out.as_mut_ptr()) };
  assert!(ok);
  unsafe { out.assume_init() }
}

fn read_limits() -> RtGcLimits {
  let mut out = core::mem::MaybeUninit::<RtGcLimits>::uninit();
  let ok = unsafe { rt_gc_get_limits(out.as_mut_ptr()) };
  assert!(ok);
  unsafe { out.assume_init() }
}

#[test]
fn gc_config_api_child() {
  let _rt = TestRuntimeGuard::new();
  if std::env::var_os("RT_GC_CONFIG_API_CHILD").is_none() {
    return;
  }

  let cfg = RtGcConfig {
    nursery_size_bytes: 256 * 1024,
    los_threshold_bytes: 8 * 1024,
    minor_gc_nursery_used_percent: 50,
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

  // Before heap initialization, getters should reflect the configured values.
  assert_eq!(read_config(), cfg);
  assert_eq!(read_limits(), limits);

  ensure_shape_table();

  // First allocation initializes the process-global heap.
  let _ = rt_alloc(256, RtShapeId(1));

  let mut young_start: *mut u8 = core::ptr::null_mut();
  let mut young_end: *mut u8 = core::ptr::null_mut();
  unsafe {
    rt_gc_get_young_range(&mut young_start, &mut young_end);
  }
  assert!(!young_start.is_null());
  assert!(!young_end.is_null());
  assert_eq!(young_end as usize - young_start as usize, cfg.nursery_size_bytes);

  // With a small nursery, we should fall back to old-gen allocation after a small number of
  // allocations.
  let mut saw_old = false;
  for _ in 0..4096 {
    let obj = rt_alloc(256, RtShapeId(1)) as usize;
    if !(young_start as usize..young_end as usize).contains(&obj) {
      saw_old = true;
      break;
    }
  }
  assert!(saw_old);

  // GC should reset the nursery, making subsequent allocations young again.
  rt_gc_collect();
  unsafe {
    rt_gc_get_young_range(&mut young_start, &mut young_end);
  }
  let after = rt_alloc(256, RtShapeId(1)) as usize;
  assert!((young_start as usize..young_end as usize).contains(&after));

  // After heap initialization, configuration must be immutable.
  assert!(!rt_gc_set_config(&cfg));
  assert!(!rt_gc_set_limits(&limits));
  assert_eq!(read_config(), cfg);
  assert_eq!(read_limits(), limits);
}

#[test]
fn gc_config_api() {
  let exe = std::env::current_exe().expect("current_exe");

  let status = Command::new(exe)
    .env("RT_GC_CONFIG_API_CHILD", "1")
    .arg("--exact")
    .arg("gc_config_api_child")
    .status()
    .expect("spawn child");

  assert!(status.success(), "expected child to exit successfully");
}

#[test]
fn gc_config_env_overrides_child() {
  let _rt = TestRuntimeGuard::new();
  if std::env::var_os("RT_GC_CONFIG_ENV_CHILD").is_none() {
    return;
  }

  // Env overrides must not be read until heap initialization.
  let before = read_config();
  assert_eq!(before.nursery_size_bytes, runtime_native::gc::HeapConfig::default().nursery_size_bytes);
  let before_limits = read_limits();
  assert_eq!(before_limits.max_heap_bytes, runtime_native::gc::HeapLimits::default().max_heap_bytes);

  ensure_shape_table();
  let _ = rt_alloc(256, RtShapeId(1));

  let after = read_config();
  assert_eq!(after.nursery_size_bytes, 1 * 1024 * 1024);

  let after_limits = read_limits();
  assert_eq!(after_limits.max_heap_bytes, 8 * 1024 * 1024);
  assert_eq!(after_limits.max_total_bytes, 16 * 1024 * 1024);
}

#[test]
fn gc_config_env_overrides() {
  let exe = std::env::current_exe().expect("current_exe");

  let status = Command::new(exe)
    .env("RT_GC_CONFIG_ENV_CHILD", "1")
    .env("ECMA_RS_GC_NURSERY_MB", "1")
    .env("ECMA_RS_GC_MAX_HEAP_MB", "8")
    .env("ECMA_RS_GC_MAX_TOTAL_MB", "16")
    .arg("--exact")
    .arg("gc_config_env_overrides_child")
    .status()
    .expect("spawn child");

  assert!(status.success(), "expected child to exit successfully");
}

#[test]
fn gc_config_rejected_after_thread_init_child() {
  let _rt = TestRuntimeGuard::new();
  if std::env::var_os("RT_GC_CONFIG_AFTER_THREAD_INIT_CHILD").is_none() {
    return;
  }

  // Thread registration eagerly initializes the process-global heap.
  rt_thread_init(3);

  let cfg = RtGcConfig {
    nursery_size_bytes: 256 * 1024,
    los_threshold_bytes: 8 * 1024,
    minor_gc_nursery_used_percent: 50,
    major_gc_old_bytes_threshold: usize::MAX,
    major_gc_old_blocks_threshold: usize::MAX,
    major_gc_external_bytes_threshold: usize::MAX,
    promote_after_minor_survivals: 1,
  };
  let limits = RtGcLimits {
    max_heap_bytes: 8 * 1024 * 1024,
    max_total_bytes: 16 * 1024 * 1024,
  };

  assert!(!rt_gc_set_config(&cfg));
  assert!(!rt_gc_set_limits(&limits));

  rt_thread_deinit();
}

#[test]
fn gc_config_rejected_after_thread_init() {
  let exe = std::env::current_exe().expect("current_exe");

  let status = Command::new(exe)
    .env("RT_GC_CONFIG_AFTER_THREAD_INIT_CHILD", "1")
    .arg("--exact")
    .arg("gc_config_rejected_after_thread_init_child")
    .status()
    .expect("spawn child");

  assert!(status.success(), "expected child to exit successfully");
}

#[test]
fn gc_config_env_overrides_do_not_override_explicit_setter_child() {
  let _rt = TestRuntimeGuard::new();
  if std::env::var_os("RT_GC_CONFIG_ENV_SETTER_CHILD").is_none() {
    return;
  }

  // Env overrides apply only when the embedder didn't set an explicit config.
  let cfg = RtGcConfig {
    nursery_size_bytes: 256 * 1024,
    los_threshold_bytes: 8 * 1024,
    minor_gc_nursery_used_percent: 50,
    major_gc_old_bytes_threshold: usize::MAX,
    major_gc_old_blocks_threshold: usize::MAX,
    major_gc_external_bytes_threshold: usize::MAX,
    promote_after_minor_survivals: 1,
  };

  assert!(rt_gc_set_config(&cfg));

  ensure_shape_table();
  let _ = rt_alloc(256, RtShapeId(1));

  let mut young_start: *mut u8 = core::ptr::null_mut();
  let mut young_end: *mut u8 = core::ptr::null_mut();
  unsafe {
    rt_gc_get_young_range(&mut young_start, &mut young_end);
  }
  assert!(!young_start.is_null());
  assert!(!young_end.is_null());

  assert_eq!(
    young_end as usize - young_start as usize,
    cfg.nursery_size_bytes,
    "env overrides must not override explicit rt_gc_set_config"
  );
}

#[test]
fn gc_config_env_overrides_do_not_override_explicit_setter() {
  let exe = std::env::current_exe().expect("current_exe");

  let status = Command::new(exe)
    .env("RT_GC_CONFIG_ENV_SETTER_CHILD", "1")
    .env("ECMA_RS_GC_NURSERY_MB", "1")
    .arg("--exact")
    .arg("gc_config_env_overrides_do_not_override_explicit_setter_child")
    .status()
    .expect("spawn child");

  assert!(status.success(), "expected child to exit successfully");
}

#[test]
fn gc_config_misaligned_ptr_aborts_child() {
  let _rt = TestRuntimeGuard::new();
  if std::env::var_os("RT_GC_CONFIG_MISALIGNED_CHILD").is_none() {
    return;
  }

  // Intentionally pass a misaligned pointer; the runtime should trap instead of triggering UB.
  let cfg = RtGcConfig {
    nursery_size_bytes: 256 * 1024,
    los_threshold_bytes: 8 * 1024,
    minor_gc_nursery_used_percent: 50,
    major_gc_old_bytes_threshold: usize::MAX,
    major_gc_old_blocks_threshold: usize::MAX,
    major_gc_external_bytes_threshold: usize::MAX,
    promote_after_minor_survivals: 1,
  };
  let misaligned = unsafe { (&cfg as *const RtGcConfig).cast::<u8>().add(1).cast::<RtGcConfig>() };

  // Expected: abort.
  let _ = rt_gc_set_config(misaligned);
}

#[test]
fn gc_config_misaligned_ptr_aborts() {
  let exe = std::env::current_exe().expect("current_exe");

  let output = Command::new(exe)
    .env("RT_GC_CONFIG_MISALIGNED_CHILD", "1")
    .arg("--exact")
    .arg("gc_config_misaligned_ptr_aborts_child")
    // Avoid losing the trap output: the Rust test harness captures per-test output in memory by
    // default. If the child process aborts, it can't flush that buffer, so the parent would see an
    // empty stderr.
    .arg("--nocapture")
    .output()
    .expect("spawn child");

  assert!(
    !output.status.success(),
    "expected misaligned rt_gc_set_config call to abort"
  );
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(
    stderr.contains("rt_gc_set_config: cfg was misaligned"),
    "expected stderr to mention misaligned cfg, got:\n{stderr}"
  );
}

#[test]
fn gc_limits_misaligned_ptr_aborts_child() {
  let _rt = TestRuntimeGuard::new();
  if std::env::var_os("RT_GC_LIMITS_MISALIGNED_CHILD").is_none() {
    return;
  }

  let limits = RtGcLimits {
    max_heap_bytes: 8 * 1024 * 1024,
    max_total_bytes: 16 * 1024 * 1024,
  };
  let misaligned = unsafe { (&limits as *const RtGcLimits).cast::<u8>().add(1).cast::<RtGcLimits>() };
  let _ = rt_gc_set_limits(misaligned);
}

#[test]
fn gc_limits_misaligned_ptr_aborts() {
  let exe = std::env::current_exe().expect("current_exe");

  let output = Command::new(exe)
    .env("RT_GC_LIMITS_MISALIGNED_CHILD", "1")
    .arg("--exact")
    .arg("gc_limits_misaligned_ptr_aborts_child")
    .arg("--nocapture")
    .output()
    .expect("spawn child");

  assert!(
    !output.status.success(),
    "expected misaligned rt_gc_set_limits call to abort"
  );
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(
    stderr.contains("rt_gc_set_limits: limits was misaligned"),
    "expected stderr to mention misaligned limits, got:\n{stderr}"
  );
}

#[test]
fn gc_get_config_misaligned_out_ptr_aborts_child() {
  let _rt = TestRuntimeGuard::new();
  if std::env::var_os("RT_GC_GET_CONFIG_MISALIGNED_CHILD").is_none() {
    return;
  }

  let mut out = core::mem::MaybeUninit::<RtGcConfig>::uninit();
  let misaligned = unsafe { (out.as_mut_ptr() as *mut u8).add(1).cast::<RtGcConfig>() };
  unsafe {
    rt_gc_get_config(misaligned);
  }
}

#[test]
fn gc_get_config_misaligned_out_ptr_aborts() {
  let exe = std::env::current_exe().expect("current_exe");

  let output = Command::new(exe)
    .env("RT_GC_GET_CONFIG_MISALIGNED_CHILD", "1")
    .arg("--exact")
    .arg("gc_get_config_misaligned_out_ptr_aborts_child")
    .arg("--nocapture")
    .output()
    .expect("spawn child");

  assert!(
    !output.status.success(),
    "expected misaligned rt_gc_get_config call to abort"
  );
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(
    stderr.contains("rt_gc_get_config: out_cfg was misaligned"),
    "expected stderr to mention misaligned out_cfg, got:\n{stderr}"
  );
}

#[test]
fn gc_get_limits_misaligned_out_ptr_aborts_child() {
  let _rt = TestRuntimeGuard::new();
  if std::env::var_os("RT_GC_GET_LIMITS_MISALIGNED_CHILD").is_none() {
    return;
  }

  let mut out = core::mem::MaybeUninit::<RtGcLimits>::uninit();
  let misaligned = unsafe { (out.as_mut_ptr() as *mut u8).add(1).cast::<RtGcLimits>() };
  unsafe {
    rt_gc_get_limits(misaligned);
  }
}

#[test]
fn gc_get_limits_misaligned_out_ptr_aborts() {
  let exe = std::env::current_exe().expect("current_exe");

  let output = Command::new(exe)
    .env("RT_GC_GET_LIMITS_MISALIGNED_CHILD", "1")
    .arg("--exact")
    .arg("gc_get_limits_misaligned_out_ptr_aborts_child")
    .arg("--nocapture")
    .output()
    .expect("spawn child");

  assert!(
    !output.status.success(),
    "expected misaligned rt_gc_get_limits call to abort"
  );
  let stderr = String::from_utf8_lossy(&output.stderr);
  assert!(
    stderr.contains("rt_gc_get_limits: out_limits was misaligned"),
    "expected stderr to mention misaligned out_limits, got:\n{stderr}"
  );
}
