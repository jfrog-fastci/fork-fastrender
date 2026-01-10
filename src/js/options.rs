use crate::error::{Error, Result};
use crate::render_control;
use std::time::Duration;
use std::time::Instant;
use vm_js::Budget as VmJsBudget;

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
/// fields are currently host-enforced and others are enforced by the active JS backend (currently
/// `vm-js`). When FastRender is built against a different JS engine, VM-specific fields may be
/// treated as best-effort/no-ops until that backend exposes equivalent budgeting hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsExecutionOptions {
  /// Bounds for how much work can be *queued* in the host event loop.
  pub event_loop_queue_limits: QueueLimits,
  /// Bounds for how much work can be *executed* in a single event loop "spin" (run).
  pub event_loop_run_limits: RunLimits,

  /// Whether the JS runtime supports executing module scripts (`<script type="module">`).
  ///
  /// When this is enabled, classic scripts with the `nomodule` attribute must be suppressed.
  pub supports_module_scripts: bool,

  /// Maximum number of bytes accepted for a single script's source text (inline or external).
  pub max_script_bytes: usize,

  /// Maximum number of simultaneously pending render-blocking stylesheets that can block
  /// parser-blocking script execution.
  pub max_pending_blocking_stylesheets: usize,

  /// VM budget: maximum number of VM instructions that may be executed before the VM is interrupted.
  ///
  /// For the `vm-js` backend, this is mapped to [`vm_js::Budget::fuel`].
  pub max_instruction_count: Option<u64>,

  /// VM budget: hard upper bound for the VM heap, in bytes.
  ///
  /// For the `vm-js` backend, this is mapped to [`vm_js::HeapLimits`]. If `None`, FastRender
  /// applies a conservative default heap cap (see [`crate::js::vm_limits::default_heap_limits`]).
  pub max_vm_heap_bytes: Option<usize>,

  /// VM budget: maximum allowed JavaScript stack depth (call frames).
  ///
  /// For the `vm-js` backend, this is mapped to [`vm_js::VmOptions::max_stack_depth`]. If `None`,
  /// the VM default is used.
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

  /// Translate these execution options into a fresh `vm-js` execution budget for "now".
  pub(crate) fn vm_js_budget_now(&self) -> VmJsBudget {
    const DEFAULT_CHECK_TIME_EVERY: u32 = 100;

    let fuel = self.max_instruction_count;

    // Use a single `Instant::now()` snapshot so "min(deadline_a, deadline_b)" decisions are
    // consistent.
    let now = Instant::now();

    // First candidate: JS-specific per-spin wall-time budget.
    let options_deadline = self
      .event_loop_run_limits
      .max_wall_time
      .and_then(|duration| now.checked_add(duration));

    // Second candidate: renderer-wide root deadline remaining time.
    let render_deadline = render_control::root_deadline().and_then(|deadline| {
      // `remaining_timeout` returns `None` both when no timeout is configured *and* when the timeout
      // has elapsed. Only treat this as an elapsed timeout when a timeout limit exists.
      if deadline.timeout_limit().is_none() {
        return None;
      }
      match deadline.remaining_timeout() {
        Some(remaining) => now.checked_add(remaining).or(Some(now)),
        None => Some(now),
      }
    });

    // Choose the earliest deadline (if any).
    let deadline = match (options_deadline, render_deadline) {
      (Some(a), Some(b)) => Some(if a <= b { a } else { b }),
      (Some(a), None) => Some(a),
      (None, Some(b)) => Some(b),
      (None, None) => None,
    };

    // When no time remains, force the VM to check the deadline on the first `tick` so we can
    // immediately abort queued work (important for microtasks and Promise jobs).
    let check_time_every = match deadline {
      Some(deadline) if deadline <= now => 1,
      _ => DEFAULT_CHECK_TIME_EVERY,
    };

    VmJsBudget {
      fuel,
      deadline,
      check_time_every,
    }
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
        max_wall_time: Some(Duration::from_millis(500)),
      },

      supports_module_scripts: false,

      // 2 MiB per script mirrors the stylesheet inlining default and keeps per-script allocations
      // bounded. Embedders can raise this when targeting real-world pages.
      max_script_bytes: 2 * 1024 * 1024,

      // Bound how many external stylesheets can block parser-inserted scripts.
      max_pending_blocking_stylesheets: 1024,

      // VM budgets (enforced by the `vm-js` backend).
      max_instruction_count: Some(50_000_000),
      // `None` means "use the embedding's default safe heap cap" (see `js::vm_limits`).
      max_vm_heap_bytes: None,
      // `None` means "use the VM's default max stack depth".
      max_stack_depth: None,
    }
  }
}
