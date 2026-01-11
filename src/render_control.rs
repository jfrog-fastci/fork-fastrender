use crate::error::{RenderError, RenderStage};
use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

thread_local! {
  static DEADLINE_STACK: RefCell<Vec<Option<RenderDeadline>>> = RefCell::new(Vec::new());
}

thread_local! {
  static ACTIVE_STAGE: Cell<Option<RenderStage>> = const { Cell::new(None) };
}

thread_local! {
  static ACTIVE_HEARTBEAT: Cell<Option<StageHeartbeat>> = const { Cell::new(None) };
}

thread_local! {
  /// Per-thread cooperative interrupt flag.
  ///
  /// This exists to bridge FastRender's render-level cancellation/deadlines into runtimes that
  /// expect a shared `Arc<AtomicBool>` interrupt primitive (e.g. `vm-js`).
  ///
  /// This flag is intentionally coarse:
  /// - It is reset to `false` when the **outermost** [`DeadlineGuard`] is installed.
  /// - It is set to `true` when a deadline check fails (timeout or external cancel).
  ///
  /// VM embeddings can clone this `Arc` and pass it into their own interrupt tokens. Once set, it
  /// remains set until the next render installs a new outermost deadline scope.
  static INTERRUPT_FLAG: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
}

// -----------------------------------------------------------------------------
// Test-only render throttling
// -----------------------------------------------------------------------------
//
// We support slowing down deadline checks to make cancellation/timeout tests deterministic.
//
// This used to be implemented by reading `FASTR_TEST_RENDER_DELAY_MS` inside every
// `RenderDeadline::check` call. That can be extremely hot (deadline checks are called from many
// inner loops), so cache the value and let tests override it programmatically.
//
// Priority:
// 1) explicit override set via `set_test_render_delay_ms`
// 2) cached env var `FASTR_TEST_RENDER_DELAY_MS`
#[cfg(any(debug_assertions, test, feature = "browser_ui"))]
thread_local! {
  // Thread-local so unit tests can safely tweak the delay without affecting other concurrently
  // executing tests (the Rust test harness reuses worker threads).
  //
  // Keep the `u64::MAX` sentinel to mirror the historical AtomicU64 implementation.
  static TEST_RENDER_DELAY_OVERRIDE_MS: Cell<u64> = const { Cell::new(u64::MAX) };
}
#[cfg(any(debug_assertions, test, feature = "browser_ui"))]
static TEST_RENDER_DELAY_ENV_CACHE_MS: AtomicU64 = AtomicU64::new(u64::MAX);

#[cfg(any(debug_assertions, test, feature = "browser_ui"))]
fn resolved_test_render_delay_ms() -> u64 {
  let override_ms = TEST_RENDER_DELAY_OVERRIDE_MS.with(|cell| cell.get());
  if override_ms != u64::MAX {
    return override_ms;
  }

  let cached = TEST_RENDER_DELAY_ENV_CACHE_MS.load(Ordering::Relaxed);
  if cached != u64::MAX {
    return cached;
  }

  let parsed = std::env::var("FASTR_TEST_RENDER_DELAY_MS")
    .ok()
    .and_then(|v| v.parse::<u64>().ok())
    .unwrap_or(0);
  TEST_RENDER_DELAY_ENV_CACHE_MS.store(parsed, Ordering::Relaxed);
  parsed
}

/// Returns the configured test render delay, in milliseconds.
///
/// This is only meaningful when compiled with `debug_assertions`, `cfg(test)`, or the `browser_ui`
/// feature. In other builds it always returns `0`.
pub fn test_render_delay_ms() -> u64 {
  #[cfg(any(debug_assertions, test, feature = "browser_ui"))]
  {
    resolved_test_render_delay_ms()
  }

  #[cfg(not(any(debug_assertions, test, feature = "browser_ui")))]
  {
    0
  }
}

/// Override the test render delay for the current thread, in milliseconds.
///
/// This affects only the current thread. Keeping it thread-local avoids flaky unit tests when the
/// Rust test harness runs multiple tests concurrently.
#[cfg(any(debug_assertions, test, feature = "browser_ui"))]
pub fn set_test_render_delay_ms(ms: Option<u64>) {
  match ms {
    Some(ms) => TEST_RENDER_DELAY_OVERRIDE_MS.with(|cell| cell.set(ms)),
    None => {
      // Clear the override and reset the env cache so a subsequent call can observe changes to the
      // process environment (useful in tests that flip the env var).
      TEST_RENDER_DELAY_OVERRIDE_MS.with(|cell| cell.set(u64::MAX));
      TEST_RENDER_DELAY_ENV_CACHE_MS.store(u64::MAX, Ordering::Relaxed);
    }
  }
}

// Keep `set_test_render_delay_ms` callable in release builds without the optional UI/test hooks
// enabled. This is primarily used by the headless UI worker loop; in optimized non-UI release
// builds we intentionally ignore the request.
#[cfg(not(any(debug_assertions, test, feature = "browser_ui")))]
pub fn set_test_render_delay_ms(_ms: Option<u64>) {}

/// Returns a per-thread shared interrupt flag suitable for VM embeddings.
pub fn interrupt_flag() -> Arc<AtomicBool> {
  INTERRUPT_FLAG.with(|flag| Arc::clone(flag))
}

fn set_interrupt_flag(value: bool) {
  INTERRUPT_FLAG.with(|flag| flag.store(value, Ordering::Relaxed));
}

/// Callback type used to cooperatively cancel rendering work.
pub type CancelCallback = dyn Fn() -> bool + Send + Sync;

/// Tracks render start time and enforces optional timeouts or external cancellation.
#[derive(Clone)]
pub struct RenderDeadline {
  start: Instant,
  timeout: Option<Duration>,
  cancel: Option<Arc<CancelCallback>>,
  allow_http_retries: bool,
}

/// Guard that installs an active deadline for the duration of a render stage.
pub struct DeadlineGuard {
  previous_len: usize,
}

/// Guard that swaps the entire per-thread deadline stack.
///
/// This is primarily used to propagate an existing deadline context (including nested deadline
/// scopes) into rayon thread pool workers where the caller thread's TLS is not visible.
pub(crate) struct DeadlineStackGuard {
  previous: Vec<Option<RenderDeadline>>,
}

/// Guard that swaps the entire per-thread stage listener stack.
///
/// Like [`DeadlineStackGuard`], this is used to propagate an existing stage listener context into
/// helper threads (e.g. the larger-stack layout thread) where the caller thread's TLS is not
/// visible.
pub(crate) struct StageListenerStackGuard {
  previous: Vec<Option<StageListener>>,
}

/// Guard that installs an active stage hint for deadline attribution.
pub struct StageGuard {
  previous: Option<RenderStage>,
}

/// Guard that installs an active stage heartbeat marker for budget attribution.
pub(crate) struct StageHeartbeatGuard {
  previous: Option<StageHeartbeat>,
}

/// Guard that installs an active stage listener for the current thread.
pub struct StageListenerGuard {
  previous_len: usize,
}

/// Stages surfaced via heartbeat callbacks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageHeartbeat {
  ReadCache,
  FollowRedirects,
  CssInline,
  DomParse,
  /// JavaScript / script execution (including event loop tasks and microtasks).
  Script,
  CssParse,
  Cascade,
  /// DOM + computed styles → box tree.
  ///
  /// The heartbeat marker string is `box_tree` (not `box_gen`) so downstream
  /// tooling can keep a stable mapping.
  BoxTree,
  Layout,
  PaintBuild,
  PaintRasterize,
  Done,
}

impl StageHeartbeat {
  const VARIANT_COUNT: usize = 12;

  fn as_index(self) -> usize {
    match self {
      StageHeartbeat::ReadCache => 0,
      StageHeartbeat::FollowRedirects => 1,
      StageHeartbeat::CssInline => 2,
      StageHeartbeat::DomParse => 3,
      StageHeartbeat::Script => 4,
      StageHeartbeat::CssParse => 5,
      StageHeartbeat::Cascade => 6,
      StageHeartbeat::BoxTree => 7,
      StageHeartbeat::Layout => 8,
      StageHeartbeat::PaintBuild => 9,
      StageHeartbeat::PaintRasterize => 10,
      StageHeartbeat::Done => 11,
    }
  }

  fn render_stage(self) -> Option<RenderStage> {
    match self {
      StageHeartbeat::ReadCache | StageHeartbeat::FollowRedirects | StageHeartbeat::DomParse => {
        Some(RenderStage::DomParse)
      }
      StageHeartbeat::Script => Some(RenderStage::Script),
      StageHeartbeat::CssInline | StageHeartbeat::CssParse => Some(RenderStage::Css),
      StageHeartbeat::Cascade => Some(RenderStage::Cascade),
      StageHeartbeat::BoxTree => Some(RenderStage::BoxTree),
      StageHeartbeat::Layout => Some(RenderStage::Layout),
      StageHeartbeat::PaintBuild | StageHeartbeat::PaintRasterize => Some(RenderStage::Paint),
      StageHeartbeat::Done => None,
    }
  }

  pub fn as_str(self) -> &'static str {
    match self {
      StageHeartbeat::ReadCache => "read_cache",
      StageHeartbeat::FollowRedirects => "follow_redirects",
      StageHeartbeat::CssInline => "css_inline",
      StageHeartbeat::DomParse => "dom_parse",
      StageHeartbeat::Script => "script",
      StageHeartbeat::CssParse => "css_parse",
      StageHeartbeat::Cascade => "cascade",
      StageHeartbeat::BoxTree => "box_tree",
      StageHeartbeat::Layout => "layout",
      StageHeartbeat::PaintBuild => "paint_build",
      StageHeartbeat::PaintRasterize => "paint_rasterize",
      StageHeartbeat::Done => "done",
    }
  }

  pub fn from_str(raw: &str) -> Option<Self> {
    match raw.trim() {
      "read_cache" => Some(StageHeartbeat::ReadCache),
      "follow_redirects" => Some(StageHeartbeat::FollowRedirects),
      "css_inline" => Some(StageHeartbeat::CssInline),
      "dom_parse" => Some(StageHeartbeat::DomParse),
      "script" => Some(StageHeartbeat::Script),
      "css_parse" => Some(StageHeartbeat::CssParse),
      "cascade" => Some(StageHeartbeat::Cascade),
      "box_tree" => Some(StageHeartbeat::BoxTree),
      "layout" => Some(StageHeartbeat::Layout),
      "paint_build" => Some(StageHeartbeat::PaintBuild),
      "paint_rasterize" => Some(StageHeartbeat::PaintRasterize),
      "done" => Some(StageHeartbeat::Done),
      _ => None,
    }
  }

  pub fn hotspot(self) -> &'static str {
    match self {
      StageHeartbeat::ReadCache | StageHeartbeat::FollowRedirects | StageHeartbeat::DomParse => {
        "fetch"
      }
      StageHeartbeat::Script => "script",
      StageHeartbeat::CssInline | StageHeartbeat::CssParse => "css",
      StageHeartbeat::Cascade => "cascade",
      StageHeartbeat::BoxTree => "box_tree",
      StageHeartbeat::Layout => "layout",
      StageHeartbeat::PaintBuild | StageHeartbeat::PaintRasterize => "paint",
      StageHeartbeat::Done => "unknown",
    }
  }

  pub fn from_render_stage(stage: RenderStage) -> Self {
    match stage {
      RenderStage::DomParse => StageHeartbeat::DomParse,
      RenderStage::Script => StageHeartbeat::Script,
      RenderStage::Css => StageHeartbeat::CssParse,
      RenderStage::Cascade => StageHeartbeat::Cascade,
      RenderStage::BoxTree => StageHeartbeat::BoxTree,
      RenderStage::Layout => StageHeartbeat::Layout,
      RenderStage::Paint => StageHeartbeat::PaintRasterize,
    }
  }
}

impl std::fmt::Display for StageHeartbeat {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str(self.as_str())
  }
}

/// Stage listener callback used by [`record_stage`].
pub type StageListener = Arc<dyn Fn(StageHeartbeat) + Send + Sync>;

thread_local! {
  static STAGE_LISTENER_STACK: RefCell<Vec<Option<StageListener>>> = RefCell::new(Vec::new());
}

thread_local! {
  /// Re-entrancy counter for [`GlobalStageListenerGuard`].
  ///
  /// Some UI layers wrap both "prepare" and "paint" in nested job helpers that each install a
  /// global stage listener guard. The stage listener itself is process-global, so we still need an
  /// inter-thread mutex to prevent concurrent installs, but we must allow the *same* thread to
  /// re-enter without deadlocking.
  static GLOBAL_STAGE_LISTENER_DEPTH: Cell<usize> = const { Cell::new(0) };
}

fn stage_listener() -> &'static Mutex<Option<StageListener>> {
  static LISTENER: OnceLock<Mutex<Option<StageListener>>> = OnceLock::new();
  LISTENER.get_or_init(|| Mutex::new(None))
}

/// Swap the global stage listener, returning the previously installed listener (if any).
///
/// `record_stage` invokes at most one listener, so callers should treat this as a global resource.
/// Prefer using [`StageListenerGuard`] to ensure the previous listener is restored even when early
/// returns occur.
pub fn swap_stage_listener(listener: Option<StageListener>) -> Option<StageListener> {
  let mut guard = stage_listener()
    .lock()
    .unwrap_or_else(|poisoned| poisoned.into_inner());
  std::mem::replace(&mut *guard, listener)
}

pub fn set_stage_listener(listener: Option<StageListener>) {
  let _ = swap_stage_listener(listener);
}

pub fn push_stage_listener(listener: Option<StageListener>) -> StageListenerGuard {
  let previous_len = STAGE_LISTENER_STACK.with(|stack| {
    let mut stack = stack.borrow_mut();
    let previous_len = stack.len();
    stack.push(listener);
    previous_len
  });
  StageListenerGuard { previous_len }
}

pub fn with_stage_listener<T>(listener: Option<StageListener>, f: impl FnOnce() -> T) -> T {
  let _guard = push_stage_listener(listener);
  f()
}

pub fn record_stage(stage: StageHeartbeat) {
  ACTIVE_STAGE.with(|active| active.set(stage.render_stage()));
  ACTIVE_HEARTBEAT.with(|active| active.set(Some(stage)));
  let maybe_thread_listener =
    STAGE_LISTENER_STACK.with(|stack| stack.borrow().last().cloned().flatten());
  if let Some(listener) = maybe_thread_listener {
    listener(stage);
  }
  let maybe_listener = {
    let guard = stage_listener()
      .lock()
      .unwrap_or_else(|poisoned| poisoned.into_inner());
    guard.as_ref().cloned()
  };
  if let Some(listener) = maybe_listener {
    listener(stage);
  }
}

/// RAII guard that installs a *global* stage listener and restores the previous listener on drop.
///
/// Prefer [`push_stage_listener`] when you only need stage events for the current thread; the
/// global listener is invoked by *all* threads.
#[must_use]
pub struct GlobalStageListenerGuard {
  previous: Option<StageListener>,
  // Hold an exclusive lock for the lifetime of this guard.
  //
  // The underlying stage listener is a *single* global callback shared by the whole process. If
  // two threads attempted to install independent `GlobalStageListenerGuard`s concurrently, the
  // later one would overwrite the listener used by the earlier job, causing stage heartbeats to be
  // mis-routed (or dropped) until one of the guards is dropped.
  //
  // The browser UI worker currently renders at most one job at a time, so serialising installs via
  // this mutex is acceptable and prevents flaky cross-test interference under `cargo test`'s
  // default parallelism.
  _exclusive: Option<std::sync::MutexGuard<'static, ()>>,
}

impl GlobalStageListenerGuard {
  pub fn new(listener: StageListener) -> Self {
    static EXCLUSIVE: OnceLock<Mutex<()>> = OnceLock::new();
    let exclusive = GLOBAL_STAGE_LISTENER_DEPTH.with(|depth| {
      let prev = depth.get();
      depth.set(prev.saturating_add(1));
      if prev == 0 {
        Some(
          EXCLUSIVE
            .get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
      } else {
        None
      }
    });
    let previous = swap_stage_listener(Some(listener));
    Self {
      previous,
      _exclusive: exclusive,
    }
  }
}

impl Drop for GlobalStageListenerGuard {
  fn drop(&mut self) {
    let previous = self.previous.take();
    let _ = swap_stage_listener(previous);
    GLOBAL_STAGE_LISTENER_DEPTH.with(|depth| {
      let cur = depth.get();
      if cur == 0 {
        // Should be impossible unless a guard is dropped without being constructed (UB) or the
        // thread-local storage was corrupted.
        return;
      }
      depth.set(cur - 1);
    });
  }
}

impl RenderDeadline {
  /// Creates a new deadline tracker starting at the current instant.
  pub fn new(timeout: Option<Duration>, cancel: Option<Arc<CancelCallback>>) -> Self {
    Self {
      start: Instant::now(),
      timeout,
      cancel,
      allow_http_retries: false,
    }
  }

  /// Returns a disabled deadline that never triggers.
  pub fn none() -> Self {
    Self::new(None, None)
  }

  /// When enabled, HTTP fetchers may honor their retry/backoff policy while still being bounded by
  /// this deadline.
  ///
  /// By default, a deadline with a timeout disables HTTP retries to avoid spending the remaining
  /// render budget on exponential backoff sleeps. Callers that want bounded retries can opt in via
  /// this flag.
  pub fn with_http_retries(mut self, enabled: bool) -> Self {
    self.allow_http_retries = enabled;
    self
  }

  /// Whether HTTP fetchers should allow retries when this deadline is active.
  pub fn http_retries_enabled(&self) -> bool {
    self.allow_http_retries
  }

  /// Returns true when either timeout or cancellation is configured.
  pub fn is_enabled(&self) -> bool {
    self.timeout.is_some() || self.cancel.is_some()
  }

  /// Elapsed time since the deadline started.
  pub fn elapsed(&self) -> Duration {
    self.start.elapsed()
  }

  /// Returns the configured timeout duration, if any.
  pub fn timeout_limit(&self) -> Option<Duration> {
    self.timeout
  }

  /// Returns the configured cooperative cancellation callback, if any.
  pub fn cancel_callback(&self) -> Option<Arc<CancelCallback>> {
    self.cancel.clone()
  }

  /// Remaining time until the configured timeout elapses, if any.
  ///
  /// Returns `None` when no timeout is configured or the deadline has already expired.
  pub fn remaining_timeout(&self) -> Option<Duration> {
    self
      .timeout
      .and_then(|limit| limit.checked_sub(self.elapsed()))
  }

  /// Check for timeout or cancellation at the given stage.
  pub fn check(&self, stage: RenderStage) -> Result<(), RenderError> {
    // `CancelCallback` does not accept any arguments, so stage-aware cancellation relies on
    // `render_control::active_stage()`. Install a scoped stage hint so callbacks (and any other
    // code invoked by this check) can observe the stage we're currently attributing to.
    let _stage_guard = StageGuard::install(Some(stage));
    #[cfg(any(debug_assertions, test, feature = "browser_ui"))]
    {
      let delay = resolved_test_render_delay_ms();
      if delay > 0 {
        std::thread::sleep(Duration::from_millis(delay));
      }
    }
    if let Some(cb) = &self.cancel {
      if cb() {
        return Err(RenderError::Timeout {
          stage,
          elapsed: self.elapsed(),
        });
      }
    }
    if let Some(limit) = self.timeout {
      let elapsed = self.elapsed();
      if elapsed >= limit {
        return Err(RenderError::Timeout { stage, elapsed });
      }
    }
    Ok(())
  }

  /// Periodically checks for timeout/cancellation every `stride` invocations.
  pub fn check_periodic(
    &self,
    counter: &mut usize,
    stride: usize,
    stage: RenderStage,
  ) -> Result<(), RenderError> {
    if !self.is_enabled() || stride == 0 {
      return Ok(());
    }
    *counter = counter.wrapping_add(1);
    if *counter % stride == 0 {
      self.check(stage)?;
    }
    Ok(())
  }
}

impl DeadlineGuard {
  /// Installs the provided deadline as the active deadline for the current thread.
  pub fn install(deadline: Option<&RenderDeadline>) -> Self {
    let cloned = deadline.cloned();
    let previous_len = DEADLINE_STACK.with(|stack| {
      let mut stack = stack.borrow_mut();
      let previous_len = stack.len();
      stack.push(cloned);
      previous_len
    });
    if previous_len == 0 {
      // Reset for each new "root" render deadline scope.
      set_interrupt_flag(false);
    }
    Self { previous_len }
  }
}

pub(crate) fn deadline_stack_snapshot() -> Vec<Option<RenderDeadline>> {
  DEADLINE_STACK.with(|stack| stack.borrow().clone())
}

pub(crate) fn stage_listener_stack_snapshot() -> Vec<Option<StageListener>> {
  STAGE_LISTENER_STACK.with(|stack| stack.borrow().clone())
}

impl DeadlineStackGuard {
  pub(crate) fn install(next: Vec<Option<RenderDeadline>>) -> Self {
    let previous = DEADLINE_STACK.with(|stack| {
      let mut stack = stack.borrow_mut();
      std::mem::replace(&mut *stack, next)
    });
    Self { previous }
  }
}
impl StageListenerStackGuard {
  pub(crate) fn install(next: Vec<Option<StageListener>>) -> Self {
    let previous = STAGE_LISTENER_STACK.with(|stack| {
      let mut stack = stack.borrow_mut();
      std::mem::replace(&mut *stack, next)
    });
    Self { previous }
  }
}

impl StageGuard {
  /// Installs the provided stage hint as the active stage for the current thread.
  pub fn install(stage: Option<RenderStage>) -> Self {
    let previous = ACTIVE_STAGE.with(|active| {
      let previous = active.get();
      active.set(stage);
      previous
    });
    Self { previous }
  }
}

impl StageHeartbeatGuard {
  pub(crate) fn install(heartbeat: Option<StageHeartbeat>) -> Self {
    let previous = ACTIVE_HEARTBEAT.with(|active| {
      let previous = active.get();
      active.set(heartbeat);
      previous
    });
    Self { previous }
  }
}

impl Drop for DeadlineGuard {
  fn drop(&mut self) {
    let previous_len = self.previous_len;
    DEADLINE_STACK.with(|stack| {
      stack.borrow_mut().truncate(previous_len);
    });
  }
}

impl Drop for StageListenerGuard {
  fn drop(&mut self) {
    let previous_len = self.previous_len;
    STAGE_LISTENER_STACK.with(|stack| {
      stack.borrow_mut().truncate(previous_len);
    });
  }
}

impl Drop for DeadlineStackGuard {
  fn drop(&mut self) {
    let previous = std::mem::take(&mut self.previous);
    DEADLINE_STACK.with(|stack| {
      *stack.borrow_mut() = previous;
    });
  }
}

impl Drop for StageListenerStackGuard {
  fn drop(&mut self) {
    let previous = std::mem::take(&mut self.previous);
    STAGE_LISTENER_STACK.with(|stack| {
      *stack.borrow_mut() = previous;
    });
  }
}

impl Drop for StageGuard {
  fn drop(&mut self) {
    ACTIVE_STAGE.with(|active| active.set(self.previous));
  }
}

impl Drop for StageHeartbeatGuard {
  fn drop(&mut self) {
    ACTIVE_HEARTBEAT.with(|active| active.set(self.previous));
  }
}

/// Returns the currently installed stage hint for this thread, if any.
pub fn active_stage() -> Option<RenderStage> {
  ACTIVE_STAGE.with(|active| active.get())
}

/// Returns the currently installed stage heartbeat marker for this thread, if any.
pub fn active_stage_heartbeat() -> Option<StageHeartbeat> {
  ACTIVE_HEARTBEAT.with(|active| active.get())
}

// -----------------------------------------------------------------------------
// Per-stage allocation budgets (best-effort)
// -----------------------------------------------------------------------------

pub(crate) struct StageAllocationBudget {
  budget_bytes: u64,
  allocated: [AtomicU64; StageHeartbeat::VARIANT_COUNT],
}

impl StageAllocationBudget {
  pub(crate) fn new(budget_bytes: u64) -> Self {
    Self {
      budget_bytes,
      allocated: std::array::from_fn(|_| AtomicU64::new(0)),
    }
  }

  pub(crate) fn budget_bytes(&self) -> u64 {
    self.budget_bytes
  }

  pub(crate) fn allocated_bytes(&self, heartbeat: StageHeartbeat) -> u64 {
    self.allocated[heartbeat.as_index()].load(Ordering::Relaxed)
  }

  pub(crate) fn reserve(&self, heartbeat: StageHeartbeat, bytes: u64) -> u64 {
    if bytes == 0 {
      return self.allocated_bytes(heartbeat);
    }
    let prev = self.allocated[heartbeat.as_index()].fetch_add(bytes, Ordering::Relaxed);
    prev.saturating_add(bytes)
  }
}

thread_local! {
  static ACTIVE_ALLOCATION_BUDGET: RefCell<Option<Arc<StageAllocationBudget>>> = RefCell::new(None);
}

pub(crate) struct StageAllocationBudgetGuard {
  previous: Option<Arc<StageAllocationBudget>>,
}

impl StageAllocationBudgetGuard {
  pub(crate) fn install(budget: Option<&Arc<StageAllocationBudget>>) -> Self {
    let next = budget.cloned();
    let previous =
      ACTIVE_ALLOCATION_BUDGET.with(|cell| std::mem::replace(&mut *cell.borrow_mut(), next));
    Self { previous }
  }
}

impl Drop for StageAllocationBudgetGuard {
  fn drop(&mut self) {
    let previous = self.previous.take();
    ACTIVE_ALLOCATION_BUDGET.with(|cell| {
      *cell.borrow_mut() = previous;
    });
  }
}

pub(crate) fn active_allocation_budget() -> Option<Arc<StageAllocationBudget>> {
  ACTIVE_ALLOCATION_BUDGET.with(|cell| cell.borrow().clone())
}

pub(crate) fn with_allocation_budget<T>(
  budget: Option<&Arc<StageAllocationBudget>>,
  f: impl FnOnce() -> T,
) -> T {
  let _guard = StageAllocationBudgetGuard::install(budget);
  f()
}

pub(crate) fn reserve_allocation(bytes: u64, context: &str) -> Result<(), RenderError> {
  if bytes == 0 {
    return Ok(());
  }
  ACTIVE_ALLOCATION_BUDGET.with(|cell| {
    let guard = cell.borrow();
    let Some(budget) = guard.as_ref() else {
      return Ok(());
    };

    let stage_opt = active_stage();
    let heartbeat_opt = active_stage_heartbeat();
    let stage = stage_opt
      .or_else(|| heartbeat_opt.and_then(|hb| hb.render_stage()))
      .unwrap_or(RenderStage::Paint);
    let heartbeat = match (stage_opt, heartbeat_opt) {
      (Some(stage), Some(heartbeat)) if heartbeat.render_stage() == Some(stage) => heartbeat,
      (Some(stage), _) => StageHeartbeat::from_render_stage(stage),
      (None, Some(heartbeat)) => heartbeat,
      (None, None) => StageHeartbeat::Done,
    };

    let allocated_bytes = budget.reserve(heartbeat, bytes);
    let budget_bytes = budget.budget_bytes();
    if allocated_bytes > budget_bytes {
      return Err(RenderError::StageAllocationBudgetExceeded {
        stage,
        heartbeat,
        allocated_bytes,
        budget_bytes,
        context: context.to_string(),
      });
    }
    Ok(())
  })
}

/// Check against any active deadline stored for the current thread.
pub fn check_active(stage: RenderStage) -> Result<(), RenderError> {
  DEADLINE_STACK.with(|stack| {
    if let Some(Some(deadline)) = stack.borrow().last() {
      match deadline.check(stage) {
        Ok(()) => Ok(()),
        Err(err) => {
          set_interrupt_flag(true);
          Err(err)
        }
      }
    } else {
      Ok(())
    }
  })
}

/// Check against the root (outermost) render deadline stored for the current thread.
///
/// Nested deadline guards are used throughout the renderer to allocate time budgets to expensive
/// phases. Resource fetch/decode operations may be triggered within those scoped budgets (e.g.
/// during display-list construction) and should generally be bounded by the overall render timeout,
/// not an internal sub-budget.
pub fn check_root(stage: RenderStage) -> Result<(), RenderError> {
  if let Some(deadline) = root_deadline() {
    match deadline.check(stage) {
      Ok(()) => Ok(()),
      Err(err) => {
        set_interrupt_flag(true);
        Err(err)
      }
    }
  } else {
    Ok(())
  }
}

/// Periodically check against any active deadline stored for the current thread.
///
/// This is a low-friction helper for hot loops: call it with a local counter and a stride
/// to amortize deadline checks while still making `RenderOptions::timeout` effective.
///
/// Example:
/// ```rust,no_run
/// # use fastrender::error::RenderStage;
/// # let mut counter = 0usize;
/// fastrender::render_control::check_active_periodic(&mut counter, 1024, RenderStage::Layout)?;
/// # Ok::<(), fastrender::error::RenderError>(())
/// ```
pub fn check_active_periodic(
  counter: &mut usize,
  stride: usize,
  stage: RenderStage,
) -> Result<(), RenderError> {
  if stride == 0 {
    return Ok(());
  }
  DEADLINE_STACK.with(|stack| {
    if let Some(Some(deadline)) = stack.borrow().last() {
      match deadline.check_periodic(counter, stride, stage) {
        Ok(()) => Ok(()),
        Err(err) => {
          set_interrupt_flag(true);
          Err(err)
        }
      }
    } else {
      Ok(())
    }
  })
}

/// Periodically check against the root render deadline stored for the current thread.
pub fn check_root_periodic(
  counter: &mut usize,
  stride: usize,
  stage: RenderStage,
) -> Result<(), RenderError> {
  if stride == 0 {
    return Ok(());
  }
  if let Some(deadline) = root_deadline() {
    match deadline.check_periodic(counter, stride, stage) {
      Ok(()) => Ok(()),
      Err(err) => {
        set_interrupt_flag(true);
        Err(err)
      }
    }
  } else {
    Ok(())
  }
}

/// Returns the currently installed deadline for this thread, if any.
pub fn active_deadline() -> Option<RenderDeadline> {
  DEADLINE_STACK.with(|stack| stack.borrow().last().cloned().flatten())
}

/// Returns the root (outermost) deadline installed for this thread, if any.
///
/// Nested deadline guards are used throughout the renderer to allocate time budgets to expensive
/// phases. Those scoped deadlines intentionally *do not* represent the overall render timeout and
/// should not be used to bound network fetches (which can be triggered deep inside a budgeted
/// phase, e.g. during display list construction).
pub fn root_deadline() -> Option<RenderDeadline> {
  DEADLINE_STACK.with(|stack| {
    for entry in stack.borrow().iter() {
      if let Some(deadline) = entry {
        return Some(deadline.clone());
      }
    }
    None
  })
}

/// Installs `deadline` for the duration of the provided closure.
pub fn with_deadline<T>(deadline: Option<&RenderDeadline>, f: impl FnOnce() -> T) -> T {
  let _guard = DeadlineGuard::install(deadline);
  f()
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn stage_heartbeat_script_roundtrips() {
    assert_eq!(
      StageHeartbeat::from_str(StageHeartbeat::Script.as_str()),
      Some(StageHeartbeat::Script)
    );
  }

  #[test]
  fn scoped_stage_listener_receives_events_and_is_removed_on_drop() {
    let stages: Arc<Mutex<Vec<StageHeartbeat>>> = Arc::new(Mutex::new(Vec::new()));
    let stages_for_listener = Arc::clone(&stages);
    {
      let _guard = push_stage_listener(Some(Arc::new(move |stage| {
        stages_for_listener.lock().unwrap().push(stage);
      })));
      record_stage(StageHeartbeat::DomParse);
      record_stage(StageHeartbeat::CssParse);
    }
    record_stage(StageHeartbeat::Cascade);
    record_stage(StageHeartbeat::Done);

    let stages = stages.lock().unwrap().clone();
    assert_eq!(
      stages,
      vec![StageHeartbeat::DomParse, StageHeartbeat::CssParse]
    );
  }

  #[test]
  fn nested_stage_listeners_restore_previous() {
    let stages_a: Arc<Mutex<Vec<StageHeartbeat>>> = Arc::new(Mutex::new(Vec::new()));
    let stages_b: Arc<Mutex<Vec<StageHeartbeat>>> = Arc::new(Mutex::new(Vec::new()));

    let stages_a_for_listener = Arc::clone(&stages_a);
    let _outer_guard = push_stage_listener(Some(Arc::new(move |stage| {
      stages_a_for_listener.lock().unwrap().push(stage);
    })));
    record_stage(StageHeartbeat::DomParse);

    {
      let stages_b_for_listener = Arc::clone(&stages_b);
      let _inner_guard = push_stage_listener(Some(Arc::new(move |stage| {
        stages_b_for_listener.lock().unwrap().push(stage);
      })));
      record_stage(StageHeartbeat::CssParse);
    }

    record_stage(StageHeartbeat::Cascade);
    drop(_outer_guard);
    record_stage(StageHeartbeat::Done);

    let stages_a = stages_a.lock().unwrap().clone();
    let stages_b = stages_b.lock().unwrap().clone();
    assert_eq!(stages_a, vec![StageHeartbeat::DomParse, StageHeartbeat::Cascade]);
    assert_eq!(stages_b, vec![StageHeartbeat::CssParse]);
  }

  #[test]
  fn thread_local_listener_does_not_affect_other_threads() {
    let stages: Arc<Mutex<Vec<StageHeartbeat>>> = Arc::new(Mutex::new(Vec::new()));
    let stages_for_listener = Arc::clone(&stages);
    let _guard = push_stage_listener(Some(Arc::new(move |stage| {
      stages_for_listener.lock().unwrap().push(stage);
    })));

    std::thread::spawn(|| record_stage(StageHeartbeat::DomParse))
      .join()
      .unwrap();

    assert!(stages.lock().unwrap().is_empty());

    let stages_thread: Arc<Mutex<Vec<StageHeartbeat>>> = Arc::new(Mutex::new(Vec::new()));
    let stages_thread_for_listener = Arc::clone(&stages_thread);
    std::thread::spawn(move || {
      {
        let _guard = push_stage_listener(Some(Arc::new(move |stage| {
          stages_thread_for_listener.lock().unwrap().push(stage);
        })));
        record_stage(StageHeartbeat::CssParse);
      }
      record_stage(StageHeartbeat::Done);
    })
    .join()
    .unwrap();

    assert_eq!(
      stages_thread.lock().unwrap().clone(),
      vec![StageHeartbeat::CssParse]
    );
    drop(_guard);
    record_stage(StageHeartbeat::Done);
  }

  #[test]
  fn deadline_check_sets_active_stage_for_cancel_callback() {
    let observed: Arc<Mutex<Vec<Option<RenderStage>>>> = Arc::new(Mutex::new(Vec::new()));
    let observed_for_cb = Arc::clone(&observed);
    let cb: Arc<CancelCallback> = Arc::new(move || {
      observed_for_cb.lock().unwrap().push(active_stage());
      true
    });

    let deadline = RenderDeadline::new(None, Some(cb));
    let outer_stage = StageGuard::install(Some(RenderStage::Layout));

    let err = deadline.check(RenderStage::Paint).expect_err("expected cancellation");
    match err {
      RenderError::Timeout { stage, .. } => assert_eq!(stage, RenderStage::Paint),
      other => panic!("unexpected error: {other:?}"),
    }

    // The deadline check must not leak its stage hint outside the check scope.
    assert_eq!(active_stage(), Some(RenderStage::Layout));
    drop(outer_stage);
    assert_eq!(active_stage(), None);

    assert_eq!(
      observed.lock().unwrap().clone(),
      vec![Some(RenderStage::Paint)]
    );
  }
}
