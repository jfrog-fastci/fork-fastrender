use super::JsExecutionOptions;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use vm_js::{HeapLimits, VmOptions};

const DEFAULT_HEAP_MAX_BYTES: usize = 64 * 1024 * 1024;
const MIN_HEAP_MAX_BYTES: usize = 4 * 1024 * 1024;

fn clamp_heap_max_bytes_to_process_limits(max_bytes: usize) -> usize {
  #[cfg(target_os = "linux")]
  {
    if let Ok((cur, _max)) = crate::process_limits::get_address_space_limit_bytes() {
      if cur > 0 && cur < u64::MAX {
        if let Ok(cur_usize) = usize::try_from(cur) {
          return max_bytes.min(cur_usize);
        }
      }
    }
  }

  max_bytes
}

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

  // If the OS-level address-space ceiling is tighter than our computed heap max (for example, a
  // sandboxed environment with a very small `RLIMIT_AS`), clamp the heap cap so `vm-js` hits its
  // own deterministic OOM path before the process runs out of virtual memory.
  max = clamp_heap_max_bytes_to_process_limits(max);

  let gc_threshold = (max / 2).min(max);
  HeapLimits::new(max, gc_threshold)
}

/// Convert [`JsExecutionOptions`] into `vm-js` heap limits.
pub fn heap_limits_from_js_options(opts: &JsExecutionOptions) -> HeapLimits {
  match opts.max_vm_heap_bytes {
    Some(max_bytes) => {
      // Respect the process-level address space ceiling (`RLIMIT_AS`) when it's tighter than the
      // caller-provided heap cap.
      let max_bytes = clamp_heap_max_bytes_to_process_limits(max_bytes);
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

#[cfg(test)]
mod tests {
  use super::*;

  #[cfg(target_os = "linux")]
  #[test]
  fn explicit_heap_limit_is_clamped_by_address_space_limit() {
    use std::process::Command;

    const CHILD_ENV: &str = "FASTR_TEST_JS_VM_LIMITS_CHILD";
    let is_child = std::env::var_os(CHILD_ENV).is_some();
    if is_child {
      let (_cur_before, max_bytes) =
        crate::process_limits::get_address_space_limit_bytes().expect("read rlimit in child");
      let max_mb = max_bytes / (1024 * 1024);
      assert!(max_mb > 0, "expected RLIMIT_AS.max to be non-zero");
      // Keep the limit large enough to comfortably run the test binary, but ensure it is finite so
      // we can verify clamping behavior.
      let desired_mb = max_mb.min(8192).max(1);
      crate::process_limits::apply_address_space_limit_mb(desired_mb)
        .expect("apply RLIMIT_AS in child process");

      let (cur_bytes, _max_bytes) =
        crate::process_limits::get_address_space_limit_bytes().expect("read rlimit after setting");
      assert!(
        cur_bytes > 0 && cur_bytes < u64::MAX,
        "expected RLIMIT_AS.cur to be a finite non-zero value"
      );
      let cur_usize = usize::try_from(cur_bytes).expect("rlimit should fit in usize");

      let opts = JsExecutionOptions {
        max_vm_heap_bytes: Some(cur_usize.saturating_mul(2)),
        ..JsExecutionOptions::default()
      };
      let limits = heap_limits_from_js_options(&opts);
      assert!(
        limits.max_bytes <= cur_usize,
        "expected heap max to be clamped to RLIMIT_AS.cur (cur={cur_usize}, heap_max={})",
        limits.max_bytes
      );
      assert_eq!(
        limits.gc_threshold,
        (limits.max_bytes / 2).min(limits.max_bytes),
        "expected gc_threshold to be derived deterministically from the effective heap cap"
      );
      return;
    }

    let exe = std::env::current_exe().expect("current test exe path");
    let test_name = "js::vm_limits::tests::explicit_heap_limit_is_clamped_by_address_space_limit";
    let output = Command::new(exe)
      .env(CHILD_ENV, "1")
      .arg("--exact")
      .arg(test_name)
      .arg("--nocapture")
      .output()
      .expect("spawn child test process");
    assert!(
      output.status.success(),
      "child process should exit successfully (stdout={}, stderr={})",
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr)
    );
  }
}
