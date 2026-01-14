use crate::error::{Error, Result};
use crate::render_control;
use std::time::Duration;
use std::time::Instant;
use vm_js::{Budget as VmJsBudget, JsString};
use webidl::WebIdlLimits;

use super::import_maps::ImportMapLimits;
use super::{QueueLimits, RunLimits};

/// Configures how much HTML parsing work is performed per event-loop "parse task".
///
/// This budget is used by streaming HTML parsing integrations (e.g. `api::BrowserTab`) to ensure
/// that:
/// - parsing yields back to the event loop regularly, and
/// - async-ready scripts (and other tasks) can interleave with parsing before EOF is reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParseBudget {
  /// Maximum number of [`crate::html::streaming_parser::StreamingHtmlParser::pump`] iterations
  /// performed in a single parse task.
  pub max_pump_iterations: usize,

  /// Optional cap on how many **UTF-8 bytes of HTML input** may be fed into the streaming parser
  /// during a single parse task.
  ///
  /// This provides a more predictable "amount of work" budget than `max_pump_iterations` alone:
  /// a single `StreamingHtmlParser::pump()` call can represent very different work depending on the
  /// structure of the buffered input.
  ///
  /// When `None` (the default), parsing is budgeted only by `max_pump_iterations` (backwards
  /// compatible).
  pub max_input_bytes_per_task: Option<usize>,
}

impl ParseBudget {
  pub fn new(max_pump_iterations: usize) -> Self {
    Self {
      max_pump_iterations: max_pump_iterations.max(1),
      max_input_bytes_per_task: None,
    }
  }
}

impl Default for ParseBudget {
  fn default() -> Self {
    // Keep tasks small so other queued tasks (e.g. async script execution) can interleave.
    Self {
      max_pump_iterations: 64,
      max_input_bytes_per_task: None,
    }
  }
}

/// Host configuration for bounding JavaScript execution.
///
/// JavaScript is hostile input. A fully safe setup typically uses *multiple* layers of limits:
/// - OS/process limits (`scripts/run_limited.sh` in this repo).
/// - Renderer-wide cooperative deadlines ([`crate::render_control::RenderDeadline`]).
/// - Host event loop limits (task/microtask/timer queue caps + per-spin run limits).
/// - VM limits (fuel/"tick" budget, heap budget, stack depth) enforced by the JS engine.
///
/// This struct is the single configuration surface for all of the above *JS-specific* limits. Some
/// fields are host-enforced (event loop queue/run limits) while others are enforced by the active
/// JS engine backend (today: `vm-js`). When FastRender is built against a different JS engine,
/// VM-specific fields may be treated as best-effort/no-ops until that backend exposes equivalent
/// budgeting hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JsExecutionOptions {
  /// Bounds for how much work can be *queued* in the host event loop.
  pub event_loop_queue_limits: QueueLimits,
  /// Bounds for how much work can be *executed* in a single event loop "spin" (run).
  pub event_loop_run_limits: RunLimits,

  /// Interval used to pace `requestAnimationFrame` callbacks ("vsync" / frame tick rate).
  ///
  /// When the host drives the event loop via a step-wise API (for example
  /// [`crate::BrowserTab::tick_frame`]), pending rAF callbacks are eligible to run at most once per
  /// interval. This is expressed in the event loop's clock domain (see [`crate::js::Clock`]).
  ///
  /// Embedding helpers like [`crate::BrowserTab::next_wake_time`] use this when the event loop is
  /// otherwise idle but has pending animation frame callbacks.
  pub animation_frame_interval: Duration,

  /// Budget for how much HTML parsing work is performed per event-loop task turn when using a
  /// streaming HTML parsing pipeline.
  pub dom_parse_budget: ParseBudget,

  /// Whether the JS runtime supports executing module scripts (`<script type="module">`).
  ///
  /// When this is enabled, classic scripts with the `nomodule` attribute must be suppressed.
  pub supports_module_scripts: bool,

  /// Maximum number of bytes accepted for a single script's source text (inline or external).
  pub max_script_bytes: usize,

  /// Deterministic resource limits for import map parsing and merging.
  ///
  /// These apply when processing `<script type="importmap">` and when embedders register import maps
  /// via `WindowHostState`.
  pub import_map_limits: ImportMapLimits,

  /// WebIDL conversion limits used by generated bindings and conversion helpers.
  ///
  /// These bounds apply to potentially unbounded conversions such as:
  /// - `DOMString`/`USVString`/etc string conversions (UTF-16 code units)
  /// - `sequence<T>` / `FrozenArray<T>` conversions (length)
  /// - `record<K, V>` conversions (entry count)
  pub webidl_limits: WebIdlLimits,

  /// Maximum number of simultaneously pending render-blocking stylesheets that can block
  /// parser-blocking script execution.
  pub max_pending_blocking_stylesheets: usize,

  /// Maximum number of bytes accepted for a single `document.write(...)`/`document.writeln(...)`
  /// call.
  ///
  /// This is a hard cap on the concatenated string that is injected into the streaming HTML parser
  /// input stream.
  pub max_document_write_bytes_per_call: usize,

  /// Maximum cumulative bytes accepted across all `document.write(...)`/`document.writeln(...)`
  /// calls within a single navigation.
  pub max_document_write_bytes_total: usize,

  /// Maximum number of `document.write(...)`/`document.writeln(...)` calls within a single
  /// navigation.
  pub max_document_write_calls: usize,

  /// Maximum number of distinct module scripts that may be fetched/parsed as part of loading a
  /// single top-level module script (including the entry module).
  pub max_module_graph_modules: usize,

  /// Maximum total number of bytes across all module sources in a single module graph load.
  ///
  /// This bounds the amount of host memory/work consumed by hostile `type="module"` pages that
  /// attempt to import enormous dependency graphs.
  pub max_module_graph_total_bytes: usize,

  /// Maximum allowed static import recursion depth when loading a module graph.
  ///
  /// The entry module has depth 0. Each `import` adds 1.
  pub max_module_graph_depth: usize,

  /// Maximum allowed length for a module specifier string, measured in UTF-16 code units.
  ///
  /// This is a host-side guard against pathological `import "<very long string>"` inputs.
  pub max_module_specifier_length: usize,

  /// VM budget: maximum `vm-js` fuel ("tick") units that may be consumed before terminating
  /// execution.
  ///
  /// Despite the name, this is **not** a literal "instruction counter". `vm-js` charges fuel in
  /// engine-defined "ticks" (see [`vm_js::Vm::tick`]): each tick consumes 1 fuel unit, but ticks are
  /// not guaranteed to correspond 1:1 with bytecode instructions or source-level operations.
  ///
  /// For the `vm-js` backend, this maps to [`vm_js::Budget::fuel`].
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

  /// Validate a module specifier against [`JsExecutionOptions::max_module_specifier_length`].
  pub fn check_module_specifier(&self, specifier: &str) -> Result<()> {
    // Module specifiers are ECMAScript Strings, which are UTF-16 code units.
    let len = specifier.encode_utf16().count();
    if len > self.max_module_specifier_length {
      return Err(Error::Other(format!(
        "Module specifier exceeded max_module_specifier_length (len={len}, limit={})",
        self.max_module_specifier_length
      )));
    }
    Ok(())
  }

  /// Validate a module specifier represented as a `vm-js` `JsString`.
  ///
  /// This performs the same check as [`Self::check_module_specifier`] but operates on UTF-16 code
  /// units directly, preserving unpaired surrogates (which are allowed in ECMAScript strings).
  pub fn check_module_specifier_js_string(&self, specifier: &JsString) -> Result<()> {
    let len = specifier.len_code_units();
    if len > self.max_module_specifier_length {
      return Err(Error::Other(format!(
        "Module specifier exceeded max_module_specifier_length (len={len}, limit={})",
        self.max_module_specifier_length
      )));
    }
    Ok(())
  }

  /// Validate a module graph recursion depth against [`JsExecutionOptions::max_module_graph_depth`].
  pub fn check_module_graph_depth(&self, depth: usize, specifier: &str) -> Result<()> {
    if depth > self.max_module_graph_depth {
      return Err(Error::Other(format!(
        "Module graph exceeded max_module_graph_depth (depth={depth}, limit={}, specifier={specifier})",
        self.max_module_graph_depth
      )));
    }
    Ok(())
  }

  /// Validate that adding a module would not exceed [`JsExecutionOptions::max_module_graph_modules`].
  pub fn check_module_graph_modules(&self, next_modules: usize, specifier: &str) -> Result<()> {
    if next_modules > self.max_module_graph_modules {
      return Err(Error::Other(format!(
        "Module graph exceeded max_module_graph_modules (next={next_modules}, limit={}, specifier={specifier})",
        self.max_module_graph_modules
      )));
    }
    Ok(())
  }

  /// Validate that adding `module_bytes` would not exceed
  /// [`JsExecutionOptions::max_module_graph_total_bytes`].
  pub fn check_module_graph_total_bytes(
    &self,
    current_total: usize,
    module_bytes: usize,
    specifier: &str,
  ) -> Result<usize> {
    let next_total = current_total
      .checked_add(module_bytes)
      .ok_or_else(|| Error::Other("Module graph total bytes overflowed usize".to_string()))?;
    if next_total > self.max_module_graph_total_bytes {
      return Err(Error::Other(format!(
        "Module graph exceeded max_module_graph_total_bytes (next={next_total}, limit={}, specifier={specifier}, module_bytes={module_bytes})",
        self.max_module_graph_total_bytes
      )));
    }
    Ok(next_total)
  }

  /// Translate these execution options into a fresh `vm-js` execution budget for "now".
  ///
  /// For the `vm-js` backend this produces a [`vm_js::Budget`] with:
  /// - [`vm_js::Budget::fuel`] = [`Self::max_instruction_count`]
  /// - [`vm_js::Budget::deadline`] = the earliest of:
  ///   - the per-spin wall-time limit ([`RunLimits::max_wall_time`]), and
  ///   - the renderer-wide root deadline ([`crate::render_control::RenderDeadline`], when set)
  pub(crate) fn vm_js_budget_now(&self) -> VmJsBudget {
    // Checking `Instant::now()` is relatively expensive for interpreter-heavy workloads (such as
    // WPT's DOM conformance tests). Use a slightly coarser-grained cadence for time checks so the
    // VM spends more time executing user code and less time polling the clock.
    const DEFAULT_CHECK_TIME_EVERY: u32 = 1_000;

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

      // ~60fps, matching typical browser vsync behavior.
      animation_frame_interval: Duration::from_nanos(16_666_667),

      dom_parse_budget: ParseBudget::default(),

      supports_module_scripts: false,

      // 2 MiB per script mirrors the stylesheet inlining default and keeps per-script allocations
      // bounded. Embedders can raise this when targeting real-world pages.
      max_script_bytes: 2 * 1024 * 1024,

      import_map_limits: ImportMapLimits::default(),

      webidl_limits: WebIdlLimits::default(),

      // `document.write` budgets (hostile-input hard caps).
      //
      // Keep per-call writes smaller than typical full HTML pages while still allowing common uses
      // like small markup injections and sync script loaders.
      max_document_write_bytes_per_call: 256 * 1024,
      // Total budget roughly matches `max_script_bytes` to keep the combined "JS + injected HTML"
      // surface bounded.
      max_document_write_bytes_total: 2 * 1024 * 1024,
      max_document_write_calls: 1024,

      // Bound how many external stylesheets can block parser-inserted scripts.
      max_pending_blocking_stylesheets: 1024,

      // Module graph budgets: safe defaults for hostile input.
      //
      // These are intentionally conservative; embedders targeting real sites will often need to
      // raise them.
      max_module_graph_modules: 1024,
      max_module_graph_total_bytes: 16 * 1024 * 1024,
      max_module_graph_depth: 64,
      max_module_specifier_length: 2048,

      // VM budgets (enforced by the `vm-js` backend).
      max_instruction_count: Some(50_000_000),
      // `None` means "use the embedding's default safe heap cap" (see `js::vm_limits`).
      max_vm_heap_bytes: None,
      // `None` means "use the VM's default max stack depth".
      max_stack_depth: None,
    }
  }
}
