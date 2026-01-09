//! Shared helpers for browser integration tests.
//!
//! Upcoming `ui_worker_*` integration tests need common building blocks:
//! - robust channel receive loops with consistent timeouts
//! - draining message bursts to reduce flakiness
//! - temp `file://` fixture creation
//! - pixmap pixel sampling for assertions
//! - concise debug formatting for `WorkerToUi` messages

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

// Default per-wait timeout used by helpers/tests that don't define their own.
//
// These integration tests do real rendering work and run in parallel by default, so allow some
// slack to avoid flakes under CPU contention.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

// Keep this small enough that long waits are still responsive, but not so small we busy-loop.
const RECV_SLICE: Duration = Duration::from_millis(25);

/// Create a deterministic `FastRender` instance for UI integration tests.
///
/// The browser UI worker tests should not depend on system-installed fonts, so always use the
/// bundled font set.
pub fn deterministic_renderer() -> fastrender::FastRender {
  fastrender::FastRender::builder()
    .font_sources(fastrender::text::font_db::FontConfig::bundled_only())
    .build()
    .expect("build deterministic renderer")
}

/// Receive from `rx` until `pred` returns `true`, or the timeout elapses.
///
/// This repeatedly calls `recv_timeout` in small slices so tests are responsive and don't get stuck
/// behind a single long `recv_timeout` call when we want to ignore unrelated messages.
pub fn recv_until<T>(
  rx: &Receiver<T>,
  timeout: Duration,
  mut pred: impl FnMut(&T) -> bool,
) -> Option<T> {
  let start = Instant::now();
  loop {
    let remaining = timeout.saturating_sub(start.elapsed());
    if remaining.is_zero() {
      return None;
    }
    let slice = remaining.min(RECV_SLICE);
    match rx.recv_timeout(slice) {
      Ok(msg) => {
        if pred(&msg) {
          return Some(msg);
        }
      }
      Err(RecvTimeoutError::Timeout) => {}
      Err(RecvTimeoutError::Disconnected) => return None,
    }
  }
}

/// Drain messages arriving on `rx` for a fixed duration.
///
/// Useful for collecting follow-up messages after sending a command, while still keeping an upper
/// bound on how long a test waits.
pub fn drain_for<T>(rx: &Receiver<T>, duration: Duration) -> Vec<T> {
  let start = Instant::now();
  let mut out = Vec::new();
  loop {
    let remaining = duration.saturating_sub(start.elapsed());
    if remaining.is_zero() {
      break;
    }
    let slice = remaining.min(RECV_SLICE);
    match rx.recv_timeout(slice) {
      Ok(msg) => out.push(msg),
      Err(RecvTimeoutError::Timeout) => {}
      Err(RecvTimeoutError::Disconnected) => break,
    }
  }
  out
}

/// Temporary filesystem-backed `file://` site for fixture-based integration tests.
pub struct TempSite {
  pub dir: tempfile::TempDir,
  /// Common base URL pointing at an `index.html` path inside `dir`.
  ///
  /// The file does not need to exist; this is typically used as the base URL for resolving relative
  /// resources like `style.css`.
  pub base_url: String,
}

impl TempSite {
  /// Create a new temporary directory with `base_url` pointing at `index.html` inside it.
  pub fn new() -> Self {
    let dir = tempfile::tempdir().expect("temp dir");
    let base_url = Self::file_url(dir.path().join("index.html"));
    Self { dir, base_url }
  }

  /// Write a file inside the temporary directory and return its `file://` URL.
  pub fn write(&self, name: &str, contents: &str) -> String {
    let path = self.dir.path().join(name);
    if let Some(parent) = path.parent() {
      std::fs::create_dir_all(parent)
        .unwrap_or_else(|err| panic!("create fixture dir {}: {err}", parent.display()));
    }
    std::fs::write(&path, contents)
      .unwrap_or_else(|err| panic!("write fixture {}: {err}", path.display()));
    Self::file_url(path)
  }

  fn file_url(path: std::path::PathBuf) -> String {
    url::Url::from_file_path(&path)
      .unwrap_or_else(|()| panic!("failed to build file:// url for {}", path.display()))
      .to_string()
  }
}

/// Sample an RGBA pixel from a pixmap.
///
/// Panics with a helpful error message if the coordinates are out of bounds.
pub fn rgba_at(pixmap: &tiny_skia::Pixmap, x: u32, y: u32) -> [u8; 4] {
  let width = pixmap.width();
  let height = pixmap.height();
  assert!(
    x < width && y < height,
    "rgba_at out of bounds: requested ({x}, {y}) in {width}x{height} pixmap"
  );
  let idx = (y as usize * width as usize + x as usize) * 4;
  let data = pixmap.data();
  [
    data[idx],
    data[idx + 1],
    data[idx + 2],
    data[idx + 3],
  ]
}

#[cfg(feature = "browser_ui")]
use fastrender::ui::messages::{NavigationReason, TabId, UiToWorker, WorkerToUi};

#[cfg(feature = "browser_ui")]
fn worker_to_ui_tab_id(msg: &WorkerToUi) -> Option<TabId> {
  if let WorkerToUi::Stage { tab_id, .. } = msg {
    return Some(*tab_id);
  }
  if let WorkerToUi::FrameReady { tab_id, .. } = msg {
    return Some(*tab_id);
  }
  if let WorkerToUi::OpenSelectDropdown { tab_id, .. } = msg {
    return Some(*tab_id);
  }
  if let WorkerToUi::SelectDropdownOpened { tab_id, .. } = msg {
    return Some(*tab_id);
  }
  if let WorkerToUi::NavigationStarted { tab_id, .. } = msg {
    return Some(*tab_id);
  }
  if let WorkerToUi::NavigationCommitted { tab_id, .. } = msg {
    return Some(*tab_id);
  }
  if let WorkerToUi::NavigationFailed { tab_id, .. } = msg {
    return Some(*tab_id);
  }
  if let WorkerToUi::ScrollStateUpdated { tab_id, .. } = msg {
    return Some(*tab_id);
  }
  if let WorkerToUi::LoadingState { tab_id, .. } = msg {
    return Some(*tab_id);
  }
  if let WorkerToUi::DebugLog { tab_id, .. } = msg {
    return Some(*tab_id);
  }
  None
}

/// Receive a `WorkerToUi` message scoped to `tab_id`.
///
/// Messages for other tabs (and any messages without a `tab_id`) are ignored.
#[cfg(feature = "browser_ui")]
pub fn recv_for_tab(
  rx: &Receiver<WorkerToUi>,
  tab_id: TabId,
  timeout: Duration,
  mut pred: impl FnMut(&WorkerToUi) -> bool,
) -> Option<WorkerToUi> {
  recv_until(rx, timeout, |msg| {
    worker_to_ui_tab_id(msg) == Some(tab_id) && pred(msg)
  })
}

/// Pretty-print a list of `WorkerToUi` messages for assertion failures.
#[cfg(feature = "browser_ui")]
pub fn format_messages(msgs: &[WorkerToUi]) -> String {
  use std::fmt::Write;

  if msgs.is_empty() {
    return "<no messages>".to_string();
  }

  let mut out = String::new();
  for (idx, msg) in msgs.iter().enumerate() {
    let _ = write!(&mut out, "{idx}: ");
    if let WorkerToUi::Stage { tab_id, stage } = msg {
      let _ = writeln!(&mut out, "Stage(tab={}, stage={stage:?})", tab_id.0);
      continue;
    }
    if let WorkerToUi::FrameReady { tab_id, frame } = msg {
      let _ = writeln!(
        &mut out,
        "FrameReady(tab={}, pixmap={}x{}, viewport_css={:?}, dpr={})",
        tab_id.0,
        frame.pixmap.width(),
        frame.pixmap.height(),
        frame.viewport_css,
        frame.dpr
      );
      continue;
    }
    if let WorkerToUi::NavigationStarted { tab_id, url } = msg {
      let _ = writeln!(&mut out, "NavigationStarted(tab={}, url={url})", tab_id.0);
      continue;
    }
    if let WorkerToUi::NavigationCommitted {
      tab_id,
      url,
      title,
      can_go_back,
      can_go_forward,
    } = msg
    {
      let _ = writeln!(
        &mut out,
        "NavigationCommitted(tab={}, url={url}, title={:?}, back={}, forward={})",
        tab_id.0,
        title,
        can_go_back,
        can_go_forward
      );
      continue;
    }
    if let WorkerToUi::NavigationFailed {
      tab_id,
      url,
      error,
    } = msg
    {
      let _ = writeln!(
        &mut out,
        "NavigationFailed(tab={}, url={url}, error={error})",
        tab_id.0
      );
      continue;
    }
    if let WorkerToUi::ScrollStateUpdated { tab_id, scroll } = msg {
      let _ = writeln!(&mut out, "ScrollStateUpdated(tab={}, scroll={scroll:?})", tab_id.0);
      continue;
    }
    if let WorkerToUi::LoadingState { tab_id, loading } = msg {
      let _ = writeln!(&mut out, "LoadingState(tab={}, loading={loading})", tab_id.0);
      continue;
    }
    if let WorkerToUi::DebugLog { tab_id, line } = msg {
      let line = line.trim_end();
      let _ = writeln!(&mut out, "DebugLog(tab={}, line={line})", tab_id.0);
      continue;
    }

    // Forward compatibility: keep this helper compiling even when `WorkerToUi` grows new variants.
    let _ = writeln!(&mut out, "{msg:?}");
  }
  out
}

// -----------------------------------------------------------------------------
// UiToWorker message constructors
// -----------------------------------------------------------------------------

/// Construct a `UiToWorker::CreateTab` message with default fields.
///
/// Centralizing this avoids churn across many integration tests when the UI/worker protocol evolves
/// (e.g. new required fields like `cancel`).
#[cfg(feature = "browser_ui")]
pub fn create_tab_msg(tab_id: TabId, initial_url: Option<String>) -> UiToWorker {
  create_tab_msg_with_cancel(tab_id, initial_url, Default::default())
}

/// Construct a `UiToWorker::CreateTab` message with an explicit cancel generation tracker.
#[cfg(feature = "browser_ui")]
pub fn create_tab_msg_with_cancel(
  tab_id: TabId,
  initial_url: Option<String>,
  cancel: fastrender::ui::cancel::CancelGens,
) -> UiToWorker {
  UiToWorker::CreateTab {
    tab_id,
    initial_url,
    cancel,
  }
}

/// Construct a `UiToWorker::CreateTab` message with sensible defaults for integration tests.
///
/// This is a convenience wrapper around [`create_tab_msg`] that accepts `&str` URLs for common
/// callsites that use string literals.
#[cfg(feature = "browser_ui")]
pub fn create_tab(tab_id: TabId, initial_url: Option<&str>) -> UiToWorker {
  create_tab_with_cancel(tab_id, initial_url, Default::default())
}

/// Like [`create_tab`], but allows the caller to provide a custom `CancelGens`.
#[cfg(feature = "browser_ui")]
pub fn create_tab_with_cancel(
  tab_id: TabId,
  initial_url: Option<&str>,
  cancel: fastrender::ui::cancel::CancelGens,
) -> UiToWorker {
  create_tab_msg_with_cancel(tab_id, initial_url.map(ToString::to_string), cancel)
}

/// Construct a `UiToWorker::ViewportChanged` message.
#[cfg(feature = "browser_ui")]
pub fn viewport_changed_msg(tab_id: TabId, viewport_css: (u32, u32), dpr: f32) -> UiToWorker {
  UiToWorker::ViewportChanged {
    tab_id,
    viewport_css,
    dpr,
  }
}

/// Construct a `UiToWorker::Navigate` message.
#[cfg(feature = "browser_ui")]
pub fn navigate_msg(tab_id: TabId, url: String, reason: NavigationReason) -> UiToWorker {
  UiToWorker::Navigate { tab_id, url, reason }
}

/// Construct a `UiToWorker::Scroll` message.
#[cfg(feature = "browser_ui")]
pub fn scroll_msg(tab_id: TabId, delta_css: (f32, f32), pointer_css: Option<(f32, f32)>) -> UiToWorker {
  UiToWorker::Scroll {
    tab_id,
    delta_css,
    pointer_css,
  }
}

#[cfg(all(test, feature = "browser_ui"))]
mod tests {
  use super::*;

  #[test]
  fn create_tab_sets_expected_fields_and_default_cancel() {
    let tab_id = TabId(123);
    let msg = create_tab(tab_id, Some("about:blank"));

    match msg {
      UiToWorker::CreateTab {
        tab_id: got_tab,
        initial_url,
        cancel,
      } => {
        assert_eq!(got_tab, tab_id);
        assert_eq!(initial_url.as_deref(), Some("about:blank"));

        let default = fastrender::ui::cancel::CancelGens::default();
        assert_eq!(cancel.snapshot_prepare(), default.snapshot_prepare());
        assert_eq!(cancel.snapshot_paint(), default.snapshot_paint());
      }
      other => panic!("expected UiToWorker::CreateTab, got {other:?}"),
    }
  }

  #[test]
  fn create_tab_preserves_none_initial_url() {
    let tab_id = TabId(7);
    let msg = create_tab(tab_id, None);

    match msg {
      UiToWorker::CreateTab {
        tab_id: got_tab,
        initial_url,
        ..
      } => {
        assert_eq!(got_tab, tab_id);
        assert!(initial_url.is_none());
      }
      other => panic!("expected UiToWorker::CreateTab, got {other:?}"),
    }
  }
}
