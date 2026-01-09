use crate::error::{Error, Result};
use std::time::Duration;

use super::{QueueLimits, RunLimits};

/// Host configuration for bounding JavaScript execution.
///
/// JavaScript is hostile input. A fully safe setup typically uses *multiple* layers of limits:
/// - OS/process limits (`scripts/run_limited.sh` in this repo).
/// - Renderer-wide cooperative deadlines ([`crate::render_control::RenderDeadline`]).
/// - Host event loop limits (task/microtask/timer queue caps + per-spin run limits).
/// - VM limits (instruction count, heap budget, stack depth) enforced by the JS engine.
///
/// This struct is the single configuration surface for all of the above *JS-specific* limits. Some
/// fields are currently host-enforced and others are placeholders until the ecma-rs VM exposes
/// budgeting hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsExecutionOptions {
  /// Bounds for how much work can be *queued* in the host event loop.
  pub event_loop_queue_limits: QueueLimits,
  /// Bounds for how much work can be *executed* in a single event loop "spin" (run).
  pub event_loop_run_limits: RunLimits,

  /// Maximum number of bytes accepted for a single script's source text (inline or external).
  pub max_script_bytes: usize,

  /// Placeholder VM budget: maximum number of VM instructions that may be executed before the VM is
  /// interrupted.
  ///
  /// Note: This is currently a no-op until the ecma-rs VM exposes an instruction counter hook.
  pub max_instruction_count: Option<u64>,

  /// Placeholder VM budget: maximum heap bytes the VM is allowed to allocate.
  ///
  /// Note: This is currently a no-op until the ecma-rs VM exposes heap limiting.
  pub max_vm_heap_bytes: Option<usize>,

  /// Placeholder VM budget: maximum allowed stack depth for JS execution.
  ///
  /// Note: This is currently a no-op until the ecma-rs VM exposes stack depth checks.
  pub max_stack_depth: Option<usize>,
}

impl JsExecutionOptions {
  /// Validate a script source payload against [`JsExecutionOptions::max_script_bytes`].
  pub fn check_script_source_bytes(&self, len: usize, context: &str) -> Result<()> {
    if len > self.max_script_bytes {
      return Err(Error::Other(format!(
        "Script source exceeded max_script_bytes (len={len}, limit={}, {context})",
        self.max_script_bytes
      )));
    }
    Ok(())
  }

  /// Validate a script source string against [`JsExecutionOptions::max_script_bytes`].
  pub fn check_script_source(&self, source: &str, context: &str) -> Result<()> {
    self.check_script_source_bytes(source.len(), context)
  }
}

impl Default for JsExecutionOptions {
  fn default() -> Self {
    // These defaults should be safe for hostile input. Real browser workloads will often want to
    // relax them, but the library default must prioritize robustness.
    Self {
      event_loop_queue_limits: QueueLimits::default(),
      event_loop_run_limits: RunLimits {
        max_tasks: 10_000,
        max_microtasks: 100_000,
        // If the embedding repeatedly calls `run_until_idle`, each call gets its own wall-time
        // budget; this is intentionally short to avoid hangs in a single "spin".
        max_wall_time: Some(Duration::from_millis(100)),
      },

      // 2 MiB per script mirrors the stylesheet inlining default and keeps per-script allocations
      // bounded. Embedders can raise this when targeting real-world pages.
      max_script_bytes: 2 * 1024 * 1024,

      // VM budgets (placeholders for ecma-rs).
      max_instruction_count: Some(50_000_000),
      max_vm_heap_bytes: Some(64 * 1024 * 1024),
      max_stack_depth: Some(1024),
    }
  }
}

