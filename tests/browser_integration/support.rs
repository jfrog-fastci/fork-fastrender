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
use fastrender::ui::messages::{TabId, WorkerToUi};

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
