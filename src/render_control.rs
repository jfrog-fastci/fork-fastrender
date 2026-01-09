use crate::error::{RenderError, RenderStage};
use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

thread_local! {
  static DEADLINE_STACK: RefCell<Vec<Option<RenderDeadline>>> = RefCell::new(Vec::new());
}

thread_local! {
  static ACTIVE_STAGE: Cell<Option<RenderStage>> = const { Cell::new(None) };
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

#[cfg(any(debug_assertions, test, feature = "browser_ui"))]
use std::sync::atomic::AtomicU64;

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
static TEST_RENDER_DELAY_OVERRIDE_MS: AtomicU64 = AtomicU64::new(u64::MAX);
#[cfg(any(debug_assertions, test, feature = "browser_ui"))]
static TEST_RENDER_DELAY_ENV_CACHE_MS: AtomicU64 = AtomicU64::new(u64::MAX);

#[cfg(any(debug_assertions, test, feature = "browser_ui"))]
fn resolved_test_render_delay_ms() -> u64 {
  let override_ms = TEST_RENDER_DELAY_OVERRIDE_MS.load(Ordering::Relaxed);
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

/// Override the global test render delay, in milliseconds.
///
/// This affects *all* threads that call `RenderDeadline::check` while compiled with
/// `debug_assertions`, `cfg(test)` or `feature = "browser_ui"`.
#[cfg(any(debug_assertions, test, feature = "browser_ui"))]
pub fn set_test_render_delay_ms(ms: Option<u64>) {
  match ms {
    Some(ms) => TEST_RENDER_DELAY_OVERRIDE_MS.store(ms, Ordering::Relaxed),
    None => {
      // Clear the override and reset the env cache so a subsequent call can observe changes to the
      // process environment (useful in tests that flip the env var).
      TEST_RENDER_DELAY_OVERRIDE_MS.store(u64::MAX, Ordering::Relaxed);
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

/// Guard that installs an active stage hint for deadline attribution.
pub struct StageGuard {
  previous: Option<RenderStage>,
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
}

/// Stage listener callback used by [`record_stage`].
pub type StageListener = Arc<dyn Fn(StageHeartbeat) + Send + Sync>;

thread_local! {
  static STAGE_LISTENER_STACK: RefCell<Vec<Option<StageListener>>> = RefCell::new(Vec::new());
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
}

impl GlobalStageListenerGuard {
  pub fn new(listener: StageListener) -> Self {
    let previous = swap_stage_listener(Some(listener));
    Self { previous }
  }
}

impl Drop for GlobalStageListenerGuard {
  fn drop(&mut self) {
    let previous = self.previous.take();
    let _ = swap_stage_listener(previous);
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

impl DeadlineStackGuard {
  pub(crate) fn install(next: Vec<Option<RenderDeadline>>) -> Self {
    let previous = DEADLINE_STACK.with(|stack| {
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

impl Drop for StageGuard {
  fn drop(&mut self) {
    ACTIVE_STAGE.with(|active| active.set(self.previous));
  }
}

/// Returns the currently installed stage hint for this thread, if any.
pub fn active_stage() -> Option<RenderStage> {
  ACTIVE_STAGE.with(|active| active.get())
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
}
