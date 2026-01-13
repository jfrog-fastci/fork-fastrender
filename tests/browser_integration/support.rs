//! Shared helpers for browser integration tests.
//!
//! Upcoming `ui_worker_*` integration tests need common building blocks:
//! - robust channel receive loops with consistent timeouts
//! - draining message bursts to reduce flakiness
//! - temp `file://` fixture creation
//! - pixmap pixel sampling for assertions
//! - concise debug formatting for `WorkerToUi` messages

use std::path::PathBuf;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use fastrender::dom2::NodeId;
use fastrender::js::{
  CurrentScriptStateHandle, EventLoop, HtmlScriptId, JsExecutionOptions, ScriptElementSpec,
  WindowRealm, WindowRealmConfig, WindowRealmHost,
};
use fastrender::{
  BrowserDocumentDom2, BrowserTabHost, BrowserTabJsExecutor, ModuleScriptExecutionStatus, Result,
};

/// Default per-wait timeout used by integration-test helpers/tests that don't define their own.
///
/// This is intentionally generous: these tests do real rendering work and can run in parallel,
/// contending for CPU.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20);

// Keep this small enough that long waits are still responsive, but not so small we busy-loop.
const RECV_SLICE: Duration = Duration::from_millis(25);

/// RAII guard for temporarily allowing `crash://` URL navigations in the UI worker.
///
/// `crash://` URLs are disabled by default so normal integration tests can't accidentally trip the
/// crash hooks. Tests that explicitly exercise crash isolation can opt in by holding this guard for
/// their duration.
#[must_use]
pub(crate) struct AllowCrashUrlsGuard {
  previous: bool,
}

impl Drop for AllowCrashUrlsGuard {
  fn drop(&mut self) {
    fastrender::ui::url::set_allow_crash_urls(self.previous);
  }
}

/// Enable the process-global `crash://` scheme allowlist for the lifetime of the returned guard.
pub(crate) fn allow_crash_urls_for_test() -> AllowCrashUrlsGuard {
  let previous = fastrender::ui::url::crash_urls_allowed();
  fastrender::ui::url::set_allow_crash_urls(true);
  AllowCrashUrlsGuard { previous }
}

pub(crate) struct ExecutorWithWindow<E> {
  inner: E,
  host_ctx: (),
  window: WindowRealm,
}

impl<E> ExecutorWithWindow<E> {
  pub(crate) fn new(inner: E) -> Self {
    let window = WindowRealm::new(WindowRealmConfig::new("https://example.invalid/"))
      .expect("create WindowRealm");
    Self {
      inner,
      host_ctx: (),
      window,
    }
  }
}

impl<E: BrowserTabJsExecutor> BrowserTabJsExecutor for ExecutorWithWindow<E> {
  fn reset_for_navigation(
    &mut self,
    document_url: Option<&str>,
    document: &mut BrowserDocumentDom2,
    current_script_state: &CurrentScriptStateHandle,
    js_execution_options: JsExecutionOptions,
  ) -> Result<()> {
    self.inner.reset_for_navigation(
      document_url,
      document,
      current_script_state,
      js_execution_options,
    )
  }

  fn execute_classic_script(
    &mut self,
    script_text: &str,
    spec: &ScriptElementSpec,
    current_script: Option<NodeId>,
    document: &mut BrowserDocumentDom2,
    event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    self
      .inner
      .execute_classic_script(script_text, spec, current_script, document, event_loop)
  }

  fn execute_module_script(
    &mut self,
    script_id: HtmlScriptId,
    script_text: &str,
    spec: &ScriptElementSpec,
    current_script: Option<NodeId>,
    document: &mut BrowserDocumentDom2,
    event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<ModuleScriptExecutionStatus> {
    self.inner.execute_module_script(
      script_id,
      script_text,
      spec,
      current_script,
      document,
      event_loop,
    )
  }

  fn execute_import_map_script(
    &mut self,
    script_text: &str,
    spec: &ScriptElementSpec,
    current_script: Option<NodeId>,
    document: &mut BrowserDocumentDom2,
    event_loop: &mut EventLoop<BrowserTabHost>,
  ) -> Result<()> {
    self
      .inner
      .execute_import_map_script(script_text, spec, current_script, document, event_loop)
  }

  fn window_realm_mut(&mut self) -> Option<&mut WindowRealm> {
    if let Some(realm) = self.inner.window_realm_mut() {
      Some(realm)
    } else {
      Some(&mut self.window)
    }
  }
}

impl<E> WindowRealmHost for ExecutorWithWindow<E> {
  fn vm_host_and_window_realm(&mut self) -> Result<(&mut dyn vm_js::VmHost, &mut WindowRealm)> {
    let ExecutorWithWindow {
      host_ctx, window, ..
    } = self;
    Ok((host_ctx, window))
  }
}

/// RAII helper for scoping the global test render delay override.
///
/// `render_control::set_test_render_delay_ms` affects the whole process, so integration tests
/// should scope it as tightly as possible and serialize execution (see `stage_listener_test_lock`).
#[must_use]
pub struct TestRenderDelayGuard;

impl TestRenderDelayGuard {
  pub fn set(ms: Option<u64>) -> Self {
    fastrender::render_control::set_test_render_delay_ms(ms);
    Self
  }
}

impl Drop for TestRenderDelayGuard {
  fn drop(&mut self) {
    fastrender::render_control::set_test_render_delay_ms(None);
  }
}

/// Pre-initialize bundled font metadata for browser UI integration tests.
///
/// The first bundled-font render can be expensive (parsing/loading many fonts). When this work
/// happens inside a worker thread, tests may time out waiting for `NavigationCommitted` /
/// `FrameReady` messages on slower CI hosts. Pre-warming it here keeps the heavy one-time cost out
/// of per-test UI worker deadlines.
#[cfg(feature = "browser_ui")]
pub(crate) fn ensure_bundled_fonts_loaded() {
  static INIT: OnceLock<()> = OnceLock::new();
  INIT.get_or_init(|| {
    let _ = fastrender::text::font_db::FontDatabase::shared_bundled_db();
  });
}

/// Create a deterministic `FastRender` instance for UI integration tests.
///
/// The browser UI worker tests should not depend on system-installed fonts, so always use a
/// deterministic fixture font set.
fn deterministic_font_config() -> fastrender::text::font_db::FontConfig {
  // Loading the full bundled fallback set is expensive; for browser integration tests we only need
  // a small, stable subset. Copy a few fixture fonts into a temporary directory and point the font
  // loader at it.
  static FONT_DIR: OnceLock<tempfile::TempDir> = OnceLock::new();

  let dir = FONT_DIR.get_or_init(|| {
    let dir = tempfile::tempdir().expect("temp font dir");
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let fonts = root.join("tests/fixtures/fonts");
    for name in [
      "NotoSans-subset.ttf",
      "NotoSerif-subset.ttf",
      "NotoSansMono-subset.ttf",
    ] {
      let src = fonts.join(name);
      let dst = dir.path().join(name);
      std::fs::copy(&src, &dst)
        .unwrap_or_else(|err| panic!("copy fixture font {}: {err}", src.display()));
    }
    dir
  });

  fastrender::text::font_db::FontConfig::new()
    .with_system_fonts(false)
    .with_bundled_fonts(false)
    .with_font_dirs([dir.path().to_path_buf()])
}

/// Create a `FastRenderBuilder` preconfigured with deterministic fixture fonts.
pub fn deterministic_renderer_builder() -> fastrender::FastRenderBuilder {
  fastrender::FastRender::builder().font_sources(deterministic_font_config())
}

pub fn deterministic_renderer() -> fastrender::FastRender {
  deterministic_renderer_builder()
    .build()
    .expect("build deterministic renderer")
}

/// Create a deterministic `FastRenderFactory` instance for integration tests.
///
/// Prefer this over `FastRenderFactory::new()` so the test suite does not depend on system-installed
/// fonts.
pub fn deterministic_factory() -> fastrender::api::FastRenderFactory {
  let renderer_config =
    fastrender::api::FastRenderConfig::default().with_font_sources(deterministic_font_config());
  fastrender::api::FastRenderFactory::with_config(
    fastrender::api::FastRenderPoolConfig::new().with_renderer_config(renderer_config),
  )
  .expect("build deterministic factory")
}

pub fn deterministic_factory_with_fetcher(
  fetcher: std::sync::Arc<dyn fastrender::resource::ResourceFetcher>,
) -> Result<fastrender::api::FastRenderFactory> {
  let renderer_config =
    fastrender::api::FastRenderConfig::default().with_font_sources(deterministic_font_config());
  fastrender::api::FastRenderFactory::with_config(
    fastrender::api::FastRenderPoolConfig::new()
      .with_renderer_config(renderer_config)
      .with_fetcher(fetcher),
  )
}

#[test]
fn browser_integration_mod_rs_has_no_init_array_env_ctor() {
  const MOD_RS: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/tests/browser_integration/mod.rs"
  ));
  for forbidden in ["init_array::init_array", "#[used", "std::env::set_var"] {
    assert!(
      !MOD_RS.contains(forbidden),
      "tests/browser_integration/mod.rs must not contain {forbidden:?}"
    );
  }

  // Avoid embedding the full env var names in this file so `rg` can be used to verify that browser
  // integration tests no longer depend on them.
  let rust_test_threads = ["RUST", "_TEST", "_THREADS"].concat();
  assert!(
    !MOD_RS.contains(&rust_test_threads),
    "tests/browser_integration/mod.rs must not contain {rust_test_threads:?}"
  );
  let bundled_fonts = ["FASTR", "_USE", "_BUNDLED", "_FONTS"].concat();
  assert!(
    !MOD_RS.contains(&bundled_fonts),
    "tests/browser_integration/mod.rs must not contain {bundled_fonts:?}"
  );
}

/// Spawn the production browser worker thread for integration tests.
///
/// Returns the raw channel endpoints and join handle for convenience.
#[cfg(feature = "browser_ui")]
pub fn spawn_browser_worker_named(
  name: impl Into<String>,
) -> (
  std::sync::mpsc::Sender<fastrender::ui::messages::UiToWorker>,
  std::sync::mpsc::Receiver<fastrender::ui::messages::WorkerToUi>,
  std::thread::JoinHandle<()>,
) {
  ensure_bundled_fonts_loaded();
  let handle = fastrender::ui::render_worker::spawn_browser_worker_with_name(name)
    .expect("spawn browser worker");
  (handle.tx, handle.rx, handle.join)
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
}

impl TempSite {
  /// Create a new temporary directory for fixture-based integration tests.
  pub fn new() -> Self {
    let dir = tempfile::tempdir().expect("temp dir");
    Self { dir }
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

/// `ResourceFetcher` implementation that only supports `file://` URLs.
///
/// This is useful for integration tests that want to exercise the production resource-fetch surface
/// (e.g. script loading) while remaining fully offline.
#[derive(Debug, Clone, Copy, Default)]
pub struct FileResourceFetcher;

impl fastrender::resource::ResourceFetcher for FileResourceFetcher {
  fn fetch(&self, url: &str) -> fastrender::Result<fastrender::resource::FetchedResource> {
    let parsed = url::Url::parse(url)
      .map_err(|err| fastrender::Error::Other(format!("invalid URL {url:?}: {err}")))?;
    if parsed.scheme() != "file" {
      return Err(fastrender::Error::Other(format!(
        "FileResourceFetcher only supports file:// URLs; got scheme={} url={url:?}",
        parsed.scheme()
      )));
    }
    let path = parsed.to_file_path().map_err(|()| {
      fastrender::Error::Other(format!("failed to convert file:// URL to path: {url:?}"))
    })?;
    let bytes = std::fs::read(&path).map_err(|err| {
      fastrender::Error::Other(format!(
        "failed to read file:// fixture resource {}: {err}",
        path.display()
      ))
    })?;
    Ok(fastrender::resource::FetchedResource::with_final_url(
      bytes,
      None,
      Some(url.to_string()),
    ))
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
  [data[idx], data[idx + 1], data[idx + 2], data[idx + 3]]
}

#[cfg(feature = "browser_ui")]
use fastrender::ui::messages::{
  KeyAction, NavigationReason, PointerButton, PointerModifiers, RepaintReason, TabId, UiToWorker,
  WorkerToUi,
};

#[cfg(feature = "browser_ui")]
fn worker_to_ui_tab_id(msg: &WorkerToUi) -> Option<TabId> {
  Some(msg.tab_id())
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

/// Wait for a `FrameReady` and `ScrollStateUpdated` pair for `tab_id`.
///
/// The worker may emit scroll updates either before or after the corresponding `FrameReady`
/// (e.g. when scroll state is reported immediately on input). This helper pairs messages by
/// matching the scroll offsets in `scroll` with `frame.scroll_state` (viewport + element offsets),
/// tolerating unrelated/stale scroll updates from earlier frames.
#[cfg(feature = "browser_ui")]
pub fn wait_for_frame_and_scroll_state_updated(
  rx: &Receiver<WorkerToUi>,
  tab_id: TabId,
  timeout: Duration,
) -> (
  fastrender::ui::messages::RenderedFrame,
  fastrender::scroll::ScrollState,
) {
  use std::collections::VecDeque;

  const MAX_PENDING: usize = 8;

  let start = Instant::now();
  let mut pending_frames: VecDeque<fastrender::ui::messages::RenderedFrame> = VecDeque::new();
  let mut pending_scrolls: VecDeque<fastrender::scroll::ScrollState> = VecDeque::new();

  loop {
    let remaining = timeout.saturating_sub(start.elapsed());
    if remaining.is_zero() {
      panic!("timed out waiting for FrameReady+ScrollStateUpdated for tab {tab_id:?}");
    }

    let slice = remaining.min(RECV_SLICE);
    match rx.recv_timeout(slice) {
      Ok(msg) => match msg {
        WorkerToUi::FrameReady { tab_id: got, frame } if got == tab_id => {
          if let Some(idx) = pending_scrolls.iter().rposition(|scroll| {
            scroll.viewport == frame.scroll_state.viewport
              && scroll.elements == frame.scroll_state.elements
          }) {
            let scroll = pending_scrolls
              .remove(idx)
              .expect("VecDeque::remove with valid idx");
            return (frame, scroll);
          }
          pending_frames.push_back(frame);
          if pending_frames.len() > MAX_PENDING {
            pending_frames.pop_front();
          }
        }
        WorkerToUi::ScrollStateUpdated {
          tab_id: got,
          scroll,
        } if got == tab_id => {
          if let Some(idx) = pending_frames.iter().rposition(|frame| {
            frame.scroll_state.viewport == scroll.viewport
              && frame.scroll_state.elements == scroll.elements
          }) {
            let frame = pending_frames
              .remove(idx)
              .expect("VecDeque::remove with valid idx");
            return (frame, scroll);
          }
          pending_scrolls.push_back(scroll);
          if pending_scrolls.len() > MAX_PENDING {
            pending_scrolls.pop_front();
          }
        }
        WorkerToUi::NavigationFailed {
          tab_id: got,
          url,
          error,
          ..
        } if got == tab_id => {
          panic!("navigation failed for {url}: {error}");
        }
        _ => {}
      },
      Err(RecvTimeoutError::Timeout) => {}
      Err(RecvTimeoutError::Disconnected) => {
        panic!("worker disconnected while waiting for frame/scroll update for tab {tab_id:?}");
      }
    }
  }
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
    if let WorkerToUi::PageAccessKitSubtree { tab_id, subtree } = msg {
      let _ = writeln!(
        &mut out,
        "PageAccessKitSubtree(tab={}, nodes={}, focus={:?})",
        tab_id.0,
        subtree.nodes.len(),
        subtree.focus_id.as_ref().map(|id| id.0.get())
      );
      continue;
    }
    if let WorkerToUi::Favicon {
      tab_id,
      width,
      height,
      rgba,
    } = msg
    {
      let _ = writeln!(
        &mut out,
        "Favicon(tab={}, size={}x{}, rgba_len={})",
        tab_id.0,
        width,
        height,
        rgba.len()
      );
      continue;
    }
    if let WorkerToUi::SelectDropdownOpened {
      tab_id,
      select_node_id,
      anchor_css,
      ..
    } = msg
    {
      let _ = writeln!(
        &mut out,
        "SelectDropdownOpened(tab={}, select_node_id={}, anchor_css={:?})",
        tab_id.0, select_node_id, anchor_css
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
        tab_id.0, title, can_go_back, can_go_forward
      );
      continue;
    }
    if let WorkerToUi::NavigationFailed {
      tab_id,
      url,
      error,
      can_go_back,
      can_go_forward,
    } = msg
    {
      let _ = writeln!(
        &mut out,
        "NavigationFailed(tab={}, url={url}, error={error}, back={}, forward={})",
        tab_id.0, can_go_back, can_go_forward
      );
      continue;
    }
    if let WorkerToUi::ScrollStateUpdated { tab_id, scroll } = msg {
      let _ = writeln!(
        &mut out,
        "ScrollStateUpdated(tab={}, scroll={scroll:?})",
        tab_id.0
      );
      continue;
    }
    if let WorkerToUi::LoadingState { tab_id, loading } = msg {
      let _ = writeln!(
        &mut out,
        "LoadingState(tab={}, loading={loading})",
        tab_id.0
      );
      continue;
    }
    if let WorkerToUi::Warning { tab_id, text } = msg {
      let _ = writeln!(&mut out, "Warning(tab={}, text={text})", tab_id.0);
      continue;
    }
    if let WorkerToUi::DebugLog { tab_id, line } = msg {
      let line = line.trim_end();
      let _ = writeln!(&mut out, "DebugLog(tab={}, line={line})", tab_id.0);
      continue;
    }
    if let WorkerToUi::SelectDropdownOpened {
      tab_id,
      select_node_id,
      control,
      anchor_css: anchor_rect_css,
    } = msg
    {
      let _ = writeln!(
        &mut out,
        "SelectDropdownOpened(tab={}, select_node_id={}, control={control:?}, anchor_css={anchor_rect_css:?})",
        tab_id.0, select_node_id
      );
      continue;
    }
    if let WorkerToUi::SelectDropdownClosed { tab_id } = msg {
      let _ = writeln!(&mut out, "SelectDropdownClosed(tab={})", tab_id.0);
      continue;
    }
    if let WorkerToUi::DateTimePickerOpened {
      tab_id,
      input_node_id,
      kind,
      value,
      anchor_css,
    } = msg
    {
      let _ = writeln!(
        &mut out,
        "DateTimePickerOpened(tab={}, input_node_id={}, kind={kind:?}, value={value:?}, anchor_css={anchor_css:?})",
        tab_id.0, input_node_id
      );
      continue;
    }
    if let WorkerToUi::DateTimePickerClosed { tab_id } = msg {
      let _ = writeln!(&mut out, "DateTimePickerClosed(tab={})", tab_id.0);
      continue;
    }
    if let WorkerToUi::ColorPickerOpened {
      tab_id,
      input_node_id,
      value,
      anchor_css,
    } = msg
    {
      let _ = writeln!(
        &mut out,
        "ColorPickerOpened(tab={}, input_node_id={}, value={value:?}, anchor_css={anchor_css:?})",
        tab_id.0, input_node_id
      );
      continue;
    }
    if let WorkerToUi::ColorPickerClosed { tab_id } = msg {
      let _ = writeln!(&mut out, "ColorPickerClosed(tab={})", tab_id.0);
      continue;
    }
    if let WorkerToUi::FilePickerOpened {
      tab_id,
      input_node_id,
      multiple,
      accept,
      anchor_css,
    } = msg
    {
      let _ = writeln!(
        &mut out,
        "FilePickerOpened(tab={}, input_node_id={}, multiple={}, accept={accept:?}, anchor_css={anchor_css:?})",
        tab_id.0, input_node_id, multiple
      );
      continue;
    }
    if let WorkerToUi::FilePickerClosed { tab_id } = msg {
      let _ = writeln!(&mut out, "FilePickerClosed(tab={})", tab_id.0);
      continue;
    }
    if let WorkerToUi::ContextMenu {
      tab_id,
      pos_css,
      default_prevented,
      link_url,
      image_url,
      can_copy,
      can_cut,
      can_paste,
      can_select_all,
    } = msg
    {
      let _ = writeln!(
        &mut out,
        "ContextMenu(tab={}, pos_css={pos_css:?}, default_prevented={default_prevented}, link_url={link_url:?}, image_url={image_url:?}, can_copy={can_copy}, can_cut={can_cut}, can_paste={can_paste}, can_select_all={can_select_all})",
        tab_id.0
      );
      continue;
    }
    if let WorkerToUi::HoverChanged {
      tab_id,
      hovered_url,
      cursor,
      tooltip,
    } = msg
    {
      let _ = writeln!(
        &mut out,
        "HoverChanged(tab={}, cursor={cursor:?}, hovered_url={:?}, tooltip={:?})",
        tab_id.0,
        hovered_url.as_deref(),
        tooltip.as_deref()
      );
      continue;
    }
    if let WorkerToUi::SetClipboardText { tab_id, text } = msg {
      let _ = writeln!(
        &mut out,
        "SetClipboardText(tab={}, text={text:?})",
        tab_id.0
      );
      continue;
    }
    if let WorkerToUi::DownloadStarted {
      tab_id,
      download_id,
      url,
      file_name,
      path,
      total_bytes,
    } = msg
    {
      let _ = writeln!(
        &mut out,
        "DownloadStarted(tab={}, id={}, url={url}, file_name={file_name:?}, path={}, total={total_bytes:?})",
        tab_id.0,
        download_id.0,
        path.display()
      );
      continue;
    }
    if let WorkerToUi::DownloadProgress {
      tab_id,
      download_id,
      received_bytes,
      total_bytes,
    } = msg
    {
      let _ = writeln!(
        &mut out,
        "DownloadProgress(tab={}, id={}, received={}, total={:?})",
        tab_id.0, download_id.0, received_bytes, total_bytes
      );
      continue;
    }
    if let WorkerToUi::DownloadFinished {
      tab_id,
      download_id,
      outcome,
    } = msg
    {
      let _ = writeln!(
        &mut out,
        "DownloadFinished(tab={}, id={}, outcome={outcome:?})",
        tab_id.0, download_id.0,
      );
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
  ensure_bundled_fonts_loaded();
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
  UiToWorker::Navigate {
    tab_id,
    url,
    reason,
  }
}

/// Construct a `UiToWorker::Scroll` message.
#[cfg(feature = "browser_ui")]
pub fn scroll_msg(
  tab_id: TabId,
  delta_css: (f32, f32),
  pointer_css: Option<(f32, f32)>,
) -> UiToWorker {
  UiToWorker::Scroll {
    tab_id,
    delta_css,
    pointer_css,
  }
}

/// Construct a `UiToWorker::ScrollTo` message.
#[cfg(feature = "browser_ui")]
pub fn scroll_to_msg(tab_id: TabId, pos_css: (f32, f32)) -> UiToWorker {
  UiToWorker::ScrollTo { tab_id, pos_css }
}

/// Construct a `UiToWorker::Scroll` message that only affects the viewport scroll position.
#[cfg(feature = "browser_ui")]
#[allow(dead_code)]
pub fn scroll_viewport(tab_id: TabId, delta_css: (f32, f32)) -> UiToWorker {
  scroll_msg(tab_id, delta_css, None)
}

/// Construct a `UiToWorker::Scroll` message scoped to the element under the pointer (if any).
#[cfg(feature = "browser_ui")]
#[allow(dead_code)]
pub fn scroll_at_pointer(
  tab_id: TabId,
  delta_css: (f32, f32),
  pointer_css: (f32, f32),
) -> UiToWorker {
  scroll_msg(tab_id, delta_css, Some(pointer_css))
}

/// Construct a `UiToWorker::PointerMove` message.
#[cfg(feature = "browser_ui")]
pub fn pointer_move(tab_id: TabId, pos_css: (f32, f32), button: PointerButton) -> UiToWorker {
  UiToWorker::PointerMove {
    tab_id,
    pos_css,
    button,
    modifiers: PointerModifiers::NONE,
  }
}

/// Construct a `UiToWorker::PointerDown` message.
#[cfg(feature = "browser_ui")]
pub fn pointer_down(tab_id: TabId, pos_css: (f32, f32), button: PointerButton) -> UiToWorker {
  UiToWorker::PointerDown {
    tab_id,
    pos_css,
    button,
    modifiers: PointerModifiers::NONE,
    click_count: 1,
  }
}

/// Construct a `UiToWorker::PointerDown` message with explicit modifier/click-count metadata.
#[cfg(feature = "browser_ui")]
pub fn pointer_down_with(
  tab_id: TabId,
  pos_css: (f32, f32),
  button: PointerButton,
  modifiers: PointerModifiers,
  click_count: u8,
) -> UiToWorker {
  UiToWorker::PointerDown {
    tab_id,
    pos_css,
    button,
    modifiers,
    click_count,
  }
}

/// Construct a `UiToWorker::PointerUp` message.
#[cfg(feature = "browser_ui")]
pub fn pointer_up(tab_id: TabId, pos_css: (f32, f32), button: PointerButton) -> UiToWorker {
  UiToWorker::PointerUp {
    tab_id,
    pos_css,
    button,
    modifiers: PointerModifiers::NONE,
  }
}

/// Construct a `UiToWorker::PointerUp` message with explicit modifier metadata.
#[cfg(feature = "browser_ui")]
pub fn pointer_up_with(
  tab_id: TabId,
  pos_css: (f32, f32),
  button: PointerButton,
  modifiers: PointerModifiers,
) -> UiToWorker {
  UiToWorker::PointerUp {
    tab_id,
    pos_css,
    button,
    modifiers,
  }
}

/// Construct a `UiToWorker::TextInput` message.
#[cfg(feature = "browser_ui")]
pub fn text_input(tab_id: TabId, text: impl Into<String>) -> UiToWorker {
  UiToWorker::TextInput {
    tab_id,
    text: text.into(),
  }
}

/// Construct a `UiToWorker::KeyAction` message.
#[cfg(feature = "browser_ui")]
pub fn key_action(tab_id: TabId, key: KeyAction) -> UiToWorker {
  UiToWorker::KeyAction { tab_id, key }
}

/// Construct a `UiToWorker::RequestRepaint` message.
#[cfg(feature = "browser_ui")]
pub fn request_repaint(tab_id: TabId, reason: RepaintReason) -> UiToWorker {
  UiToWorker::RequestRepaint { tab_id, reason }
}

#[cfg(all(test, feature = "browser_ui"))]
mod tests {
  use super::*;

  #[test]
  fn deterministic_renderer_builds() {
    let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
    let _ = deterministic_renderer();
  }

  #[test]
  fn create_tab_sets_expected_fields_and_default_cancel() {
    let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
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
    let _browser_integration_lock = crate::browser_integration::stage_listener_test_lock();
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
  #[test]
  fn browser_integration_uses_deterministic_font_config() {
    let config = deterministic_font_config();
    assert!(
      !config.use_system_fonts,
      "deterministic browser integration tests must not depend on system font discovery"
    );
    assert!(
      !config.use_bundled_fonts,
      "deterministic browser integration tests should use the fixture font subset"
    );
    assert!(
      !config.font_dirs.is_empty(),
      "deterministic font config should load fonts from a fixture directory"
    );
  }
}
