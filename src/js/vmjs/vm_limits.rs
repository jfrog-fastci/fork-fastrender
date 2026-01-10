use super::JsExecutionOptions;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use vm_js::{HeapLimits, VmOptions};

const DEFAULT_HEAP_MAX_BYTES: usize = 64 * 1024 * 1024;
const MIN_HEAP_MAX_BYTES: usize = 4 * 1024 * 1024;

/// Derive a conservative default `vm-js` heap budget.
///
/// This keeps JavaScript heaps bounded even when the embedding does not provide an explicit cap.
/// When FastRender is run under an `RLIMIT_AS`/cgroup memory ceiling, the heap cap is scaled down so
/// other renderer subsystems still have headroom.
pub fn default_heap_limits() -> HeapLimits {
  let mut max = DEFAULT_HEAP_MAX_BYTES;

  // If the process is constrained by `RLIMIT_AS` (typically applied by FastRender CLI flags or
  // an outer `prlimit`/cgroup), keep JS heap usage to a small fraction of that ceiling so other
  // renderer subsystems still have headroom.
  #[cfg(target_os = "linux")]
  {
    if let Ok((cur, _max)) = crate::process_limits::get_address_space_limit_bytes() {
      if cur > 0 && cur < u64::MAX {
        let suggested = cur / 8;
        if let Ok(suggested) = usize::try_from(suggested) {
          max = max.min(suggested.max(MIN_HEAP_MAX_BYTES));
        }
      }
    }
  }

  let gc_threshold = (max / 2).min(max);
  HeapLimits::new(max, gc_threshold)
}

/// Convert [`JsExecutionOptions`] into `vm-js` heap limits.
pub fn heap_limits_from_js_options(opts: &JsExecutionOptions) -> HeapLimits {
  match opts.max_vm_heap_bytes {
    Some(max_bytes) => {
      let gc_threshold = (max_bytes / 2).min(max_bytes);
      HeapLimits::new(max_bytes, gc_threshold)
    }
    None => default_heap_limits(),
  }
}

/// Convert [`JsExecutionOptions`] into `vm-js` construction-time VM options.
pub fn vm_options_from_js_options(
  opts: &JsExecutionOptions,
  interrupt_flag: Option<Arc<AtomicBool>>,
) -> VmOptions {
  let mut vm_options = VmOptions::default();
  vm_options.max_stack_depth = opts.max_stack_depth.unwrap_or(vm_options.max_stack_depth);
  vm_options.interrupt_flag = interrupt_flag;
  vm_options
}

