use crate::geometry::Rect;
use crate::render_control::StageHeartbeat;
use crate::scroll::ScrollBounds;
use crate::scroll::ScrollState;
use crate::tree::box_tree::SelectControl;
use crate::ui::cancel::CancelGens;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(test)]
use std::sync::Mutex;

pub use crate::interaction::KeyAction;
use tiny_skia::Pixmap;

static NEXT_TAB_ID: AtomicU64 = AtomicU64::new(1);

#[cfg(test)]
pub(crate) static TAB_ID_TEST_LOCK: Mutex<()> = Mutex::new(());

/// Identifier for a browser UI tab.
///
/// This is kept as a thin wrapper to avoid mixing tab ids with other identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TabId(pub u64);

impl TabId {
  /// Generate a new process-unique tab id.
  ///
  /// Intended for UI thread use when creating new tabs.
  pub fn new() -> Self {
    // `fetch_add` returns the previous value.
    //
    // `0` is reserved as an "invalid" `TabId` value, so the counter starts at 1. In the
    // astronomically unlikely event that we wrap around `u64::MAX` (requiring ~1.8e19 allocations
    // in a single process), skip over 0 and keep going rather than panicking.
    loop {
      let id = NEXT_TAB_ID.fetch_add(1, Ordering::Relaxed);
      if id != 0 {
        return Self(id);
      }
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NavigationReason {
  TypedUrl,
  LinkClick,
  BackForward,
  Reload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RepaintReason {
  Explicit,
  ViewportChanged,
  Scroll,
  Input,
  Navigation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PointerButton {
  None,
  Primary,
  Secondary,
  Middle,
  Back,
  Forward,
  Other(u16),
}

/// Snapshot of modifier keys/buttons active during a pointer event.
///
/// This is part of the UI↔worker protocol, so it must remain small, `Copy`, and independent of any
/// specific windowing backend types (e.g. winit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct PointerModifiers(u8);

impl PointerModifiers {
  pub const NONE: Self = Self(0);
  pub const CTRL: Self = Self(1 << 0);
  pub const SHIFT: Self = Self(1 << 1);
  pub const ALT: Self = Self(1 << 2);
  pub const META: Self = Self(1 << 3);

  /// Cross-platform "command" modifier (Cmd on macOS, Ctrl elsewhere).
  ///
  /// This is useful for browser-style gestures like Cmd/Ctrl-click to open links in a new tab.
  pub fn command(self) -> bool {
    if cfg!(target_os = "macos") {
      self.meta()
    } else {
      self.ctrl()
    }
  }

  pub fn ctrl(self) -> bool {
    (self.0 & Self::CTRL.0) != 0
  }

  pub fn shift(self) -> bool {
    (self.0 & Self::SHIFT.0) != 0
  }

  pub fn alt(self) -> bool {
    (self.0 & Self::ALT.0) != 0
  }

  pub fn meta(self) -> bool {
    (self.0 & Self::META.0) != 0
  }
}

impl std::ops::BitOr for PointerModifiers {
  type Output = Self;

  fn bitor(self, rhs: Self) -> Self::Output {
    Self(self.0 | rhs.0)
  }
}

impl std::ops::BitOrAssign for PointerModifiers {
  fn bitor_assign(&mut self, rhs: Self) {
    self.0 |= rhs.0;
  }
}

/// Scroll sizing information for the root scroll container (viewport).
///
/// This is intended for UI layers that want to draw scrollbars or otherwise surface scroll state
/// without having to re-run layout queries.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScrollMetrics {
  /// Viewport size in CSS pixels (matches [`RenderedFrame::viewport_css`]).
  pub viewport_css: (u32, u32),
  /// Current viewport scroll offset in CSS pixels (matches `RenderedFrame.scroll_state.viewport`).
  pub scroll_css: (f32, f32),
  /// Scroll bounds for the root scroll container in CSS pixels.
  ///
  /// For typical documents this is `min_* = 0` and `max_* = content_* - viewport_*`.
  pub bounds_css: ScrollBounds,
  /// Content size for the root scroll container in CSS pixels.
  pub content_css: (f32, f32),
}

/// High-level pointer cursor semantics reported by the render worker.
///
/// This intentionally mirrors a small subset of common browser cursor types so UIs can map them to
/// platform cursor icons.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CursorKind {
  Default,
  Pointer,
  Text,
  Crosshair,
  NotAllowed,
  Grab,
  Grabbing,
}

impl Default for CursorKind {
  fn default() -> Self {
    CursorKind::Default
  }
}

/// An owned rendered frame produced by the render worker.
///
/// This owns the underlying pixel buffer (`tiny_skia::Pixmap`) and is expected to be sent to the
/// UI thread by move (avoid cloning large pixmaps).
pub struct RenderedFrame {
  pub pixmap: Pixmap,
  pub viewport_css: (u32, u32),
  pub dpr: f32,
  pub scroll_state: ScrollState,
  pub scroll_metrics: ScrollMetrics,
  /// True when the rendered document contains time-based effects (CSS animations/transitions).
  ///
  /// Front-ends that want animated content should drive periodic [`UiToWorker::Tick`] messages for
  /// the active tab while this is `true`.
  pub wants_ticks: bool,
}

impl std::fmt::Debug for RenderedFrame {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("RenderedFrame")
      .field("pixmap_px", &(self.pixmap.width(), self.pixmap.height()))
      .field("viewport_css", &self.viewport_css)
      .field("dpr", &self.dpr)
      .field("scroll_state", &self.scroll_state)
      .field("scroll_metrics", &self.scroll_metrics)
      .field("wants_ticks", &self.wants_ticks)
      .finish()
  }
}

/// Messages sent from the UI thread to the render worker.
#[derive(Debug)]
pub enum UiToWorker {
  CreateTab {
    tab_id: TabId,
    /// Optional URL to navigate immediately after creating the tab.
    ///
    /// When `None`, the tab is created in an "empty" state and will not produce any navigation or
    /// frame messages until the UI sends an explicit [`UiToWorker::Navigate`].
    ///
    /// UIs that want a default page (for example `about:newtab`) should provide it explicitly.
    initial_url: Option<String>,
    /// Per-tab cancellation generations shared with the UI thread.
    ///
    /// The UI should retain a clone of this `CancelGens` in its per-tab model and bump gens
    /// *before* sending new actions so long-running prepare/layout/paint work can be cooperatively
    /// cancelled mid-flight.
    cancel: CancelGens,
  },
  /// Optional alias for [`UiToWorker::CreateTab`].
  ///
  /// Kept for protocol flexibility as the browser UI evolves.
  NewTab {
    tab_id: TabId,
    /// Optional URL to navigate immediately after creating the tab.
    ///
    /// See [`UiToWorker::CreateTab`] for semantics.
    initial_url: Option<String>,
  },
  CloseTab {
    tab_id: TabId,
  },
  SetActiveTab {
    tab_id: TabId,
  },
  /// Set the directory used for downloaded files.
  ///
  /// Front-ends should send this once during startup (before the first navigation) so downloads use
  /// the expected directory.
  SetDownloadDirectory {
    path: PathBuf,
  },
  /// Navigate to a new URL (typed in the address bar or clicked on the page).
  Navigate {
    tab_id: TabId,
    url: String,
    reason: NavigationReason,
  },
  /// Navigate to the previous history entry for this tab.
  ///
  /// The worker owns history state, so the UI does not provide a URL.
  GoBack {
    tab_id: TabId,
  },
  /// Navigate to the next history entry for this tab.
  ///
  /// The worker owns history state, so the UI does not provide a URL.
  GoForward {
    tab_id: TabId,
  },
  /// Reload the current history entry for this tab.
  ///
  /// The worker owns history state, so the UI does not provide a URL.
  Reload {
    tab_id: TabId,
  },
  /// Periodic "tick" from the UI thread used to drive the tab's event loop and repaint pipeline.
  ///
  /// The render worker should execute a bounded slice of JS/event-loop work (timers, microtasks)
  /// and, if the tab becomes dirty, render a new frame.
  Tick {
    tab_id: TabId,
  },
  ViewportChanged {
    tab_id: TabId,
    viewport_css: (u32, u32),
    dpr: f32,
  },
  Scroll {
    tab_id: TabId,
    delta_css: (f32, f32),
    /// Pointer position in **viewport-local CSS pixels** (0,0 at the top-left of the rendered
    /// viewport).
    ///
    /// This coordinate does **not** include the current scroll offset (`ScrollState.viewport`).
    /// The worker is responsible for converting viewport-local coords to page coords by adding the
    /// current scroll offset.
    pointer_css: Option<(f32, f32)>,
  },
  /// Scroll the viewport to an absolute position (in CSS px).
  ///
  /// Unlike [`UiToWorker::Scroll`], this does not attempt to target element scroll containers under
  /// the pointer; it is intended for UI scrollbars and programmatic scrolling.
  ScrollTo {
    tab_id: TabId,
    pos_css: (f32, f32),
  },
  PointerMove {
    tab_id: TabId,
    /// Pointer position in **viewport-local CSS pixels** (0,0 at the top-left of the rendered
    /// viewport).
    ///
    /// This coordinate does **not** include the current scroll offset (`ScrollState.viewport`).
    /// The worker is responsible for converting viewport-local coords to page coords by adding the
    /// current scroll offset.
    pos_css: (f32, f32),
    button: PointerButton,
    modifiers: PointerModifiers,
  },
  PointerDown {
    tab_id: TabId,
    /// Pointer position in **viewport-local CSS pixels** (0,0 at the top-left of the rendered
    /// viewport).
    ///
    /// This coordinate does **not** include the current scroll offset (`ScrollState.viewport`).
    /// The worker is responsible for converting viewport-local coords to page coords by adding the
    /// current scroll offset.
    pos_css: (f32, f32),
    button: PointerButton,
    modifiers: PointerModifiers,
    /// Consecutive click count reported by the UI layer (1 = single click, 2 = double click, ...).
    ///
    /// This is used for browser-style text selection gestures in `<input>`/`<textarea>` (double
    /// click selects a word; triple click selects a line / all).
    ///
    /// UIs that do not implement multi-click detection should always send `1`.
    click_count: u8,
  },
  PointerUp {
    tab_id: TabId,
    /// Pointer position in **viewport-local CSS pixels** (0,0 at the top-left of the rendered
    /// viewport).
    ///
    /// This coordinate does **not** include the current scroll offset (`ScrollState.viewport`).
    /// The worker is responsible for converting viewport-local coords to page coords by adding the
    /// current scroll offset.
    pos_css: (f32, f32),
    button: PointerButton,
    modifiers: PointerModifiers,
  },
  /// Request a page hit-test for context menu purposes.
  ///
  /// The worker responds with [`WorkerToUi::ContextMenu`] containing a resolved link URL (if the
  /// hit target is a link).
  ContextMenuRequest {
    tab_id: TabId,
    /// Pointer position in **viewport-local CSS pixels** (0,0 at the top-left of the rendered
    /// viewport).
    ///
    /// This coordinate does **not** include the current scroll offset (`ScrollState.viewport`).
    /// The worker is responsible for converting viewport-local coords to page coords by adding the
    /// current scroll offset.
    pos_css: (f32, f32),
  },
  /// User chose an option in a dropdown `<select>` popup.
  ///
  /// The UI should send this after receiving [`WorkerToUi::SelectDropdownOpened`].
  ///
  /// Workers typically respond with [`WorkerToUi::SelectDropdownClosed`] so UIs can dismiss the
  /// popup deterministically, even when the selection is a no-op (choosing the already-selected
  /// option).
  SelectDropdownChoose {
    tab_id: TabId,
    select_node_id: usize,
    option_node_id: usize,
  },
  /// User dismissed an open dropdown `<select>` popup without choosing an option.
  ///
  /// Front-ends typically send this when the user presses Escape or clicks outside the popup.
  /// Workers may treat this as a no-op or use it to emit [`WorkerToUi::SelectDropdownClosed`].
  SelectDropdownCancel {
    tab_id: TabId,
  },
  TextInput {
    tab_id: TabId,
    text: String,
  },
  /// IME preedit update for the focused page text control (input/textarea).
  ///
  /// This represents an in-progress composition string that should be rendered at the caret
  /// position but **not** committed to the DOM value until an [`UiToWorker::ImeCommit`] arrives.
  ImePreedit {
    tab_id: TabId,
    text: String,
    /// Cursor/selection range within `text`, when provided by the platform IME.
    cursor: Option<(usize, usize)>,
  },
  /// IME commit for the focused page text control (input/textarea).
  ///
  /// This is the final committed text from the platform IME and should be inserted into the DOM
  /// value at the caret.
  ImeCommit {
    tab_id: TabId,
    text: String,
  },
  /// Cancels any active IME composition for the focused page text control.
  ImeCancel {
    tab_id: TabId,
  },
  /// Paste text into the currently focused text control (input/textarea).
  ///
  /// The worker is responsible for applying the text at the caret, replacing any selection, and
  /// respecting inert/disabled/readonly rules.
  Paste {
    tab_id: TabId,
    text: String,
  },
  /// Copy the current selection in the focused text control (input/textarea) to the clipboard.
  ///
  /// This should not mutate the DOM; workers respond by sending
  /// [`WorkerToUi::SetClipboardText`].
  Copy {
    tab_id: TabId,
  },
  /// Cut the current selection in the focused text control (input/textarea) to the clipboard.
  ///
  /// Workers respond by sending [`WorkerToUi::SetClipboardText`], and delete the selection when
  /// the control is editable.
  Cut {
    tab_id: TabId,
  },
  /// Select all text in the currently focused text control (input/textarea).
  SelectAll {
    tab_id: TabId,
  },
  KeyAction {
    tab_id: TabId,
    key: KeyAction,
  },
  /// Begin/update an active "find in page" query for this tab.
  ///
  /// An empty query clears all find highlights/results for the tab.
  FindQuery {
    tab_id: TabId,
    query: String,
    case_sensitive: bool,
  },
  /// Jump to the next match for the active find query.
  FindNext {
    tab_id: TabId,
  },
  /// Jump to the previous match for the active find query.
  FindPrev {
    tab_id: TabId,
  },
  /// Explicitly stop/close find in page for this tab (clears highlights/results).
  ///
  /// This is equivalent to sending [`UiToWorker::FindQuery`] with an empty query, but is exposed as
  /// a distinct message for clearer front-end UX and intent.
  FindStop {
    tab_id: TabId,
  },
  RequestRepaint {
    tab_id: TabId,
    reason: RepaintReason,
  },
  SelectDropdownPick {
    tab_id: TabId,
    select_node_id: usize,
    item_index: usize,
  },
}

impl UiToWorker {
  pub fn select_dropdown_choose(
    tab_id: TabId,
    select_node_id: usize,
    option_node_id: usize,
  ) -> Self {
    UiToWorker::SelectDropdownChoose {
      tab_id,
      select_node_id,
      option_node_id,
    }
  }

  pub fn select_dropdown_cancel(tab_id: TabId) -> Self {
    UiToWorker::SelectDropdownCancel { tab_id }
  }
}

/// Messages sent from the render worker to the UI thread.
#[derive(Debug)]
pub enum WorkerToUi {
  /// Coarse-grained stage heartbeat emitted while preparing or painting a document.
  Stage {
    tab_id: TabId,
    stage: StageHeartbeat,
  },
  /// Best-effort favicon for the currently committed page.
  ///
  /// The payload is a small premultiplied RGBA8 buffer intended for immediate upload into a UI
  /// texture (e.g. wgpu). Workers should keep this bounded (small dimensions and byte length).
  Favicon {
    tab_id: TabId,
    rgba: Vec<u8>,
    width: u32,
    height: u32,
  },
  FrameReady {
    tab_id: TabId,
    frame: RenderedFrame,
  },
  OpenSelectDropdown {
    tab_id: TabId,
    select_node_id: usize,
    control: SelectControl,
  },
  NavigationStarted {
    tab_id: TabId,
    url: String,
  },
  NavigationCommitted {
    tab_id: TabId,
    url: String,
    title: Option<String>,
    can_go_back: bool,
    can_go_forward: bool,
  },
  NavigationFailed {
    tab_id: TabId,
    url: String,
    error: String,
    can_go_back: bool,
    can_go_forward: bool,
  },
  /// Request that the UI open `url` in a new tab (e.g. for `<a target="_blank">` or
  /// Ctrl/Cmd-click/middle-click).
  ///
  /// The worker does not allocate `TabId`s; the UI owns tab identity and is responsible for
  /// creating a new tab and issuing the corresponding [`UiToWorker::CreateTab`] +
  /// [`UiToWorker::Navigate`] messages.
  RequestOpenInNewTab {
    tab_id: TabId,
    url: String,
  },
  ScrollStateUpdated {
    tab_id: TabId,
    scroll: ScrollState,
  },
  LoadingState {
    tab_id: TabId,
    loading: bool,
  },
  /// Non-fatal warning intended for user-facing display (e.g. viewport clamping).
  Warning {
    tab_id: TabId,
    text: String,
  },
  DebugLog {
    tab_id: TabId,
    line: String,
  },
  SelectDropdownOpened {
    tab_id: TabId,
    select_node_id: usize,
    control: SelectControl,
    /// Bounding box of the `<select>` control in **viewport CSS coordinates**.
    ///
    /// (0,0 is the top-left of the rendered viewport; does not include scroll offset.)
    anchor_css: Rect,
  },
  SelectDropdownClosed {
    tab_id: TabId,
  },
  /// Response to [`UiToWorker::ContextMenuRequest`].
  ContextMenu {
    tab_id: TabId,
    /// Pointer position in viewport-local CSS pixels as provided by the UI request.
    pos_css: (f32, f32),
    /// Fully-resolved link URL under the cursor, if any.
    link_url: Option<String>,
  },
  /// Hover metadata changed for a tab (cursor semantics and hovered link URL).
  ///
  /// Workers should only emit this message when the hover state actually changes (deduped) to keep
  /// the protocol lightweight on high-frequency pointer move streams.
  HoverChanged {
    tab_id: TabId,
    hovered_url: Option<String>,
    cursor: CursorKind,
  },
  /// Updated find-in-page results for a tab.
  ///
  /// - `active_match_index` is **0-based** (UIs can display `+1`).
  /// - When `match_count == 0`, `active_match_index` must be `None`.
  FindResult {
    tab_id: TabId,
    query: String,
    case_sensitive: bool,
    match_count: usize,
    active_match_index: Option<usize>,
  },
  /// Request that the UI set the OS clipboard text.
  ///
  /// This is typically emitted in response to [`UiToWorker::Copy`] or [`UiToWorker::Cut`].
  SetClipboardText {
    tab_id: TabId,
    text: String,
  },
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::collections::HashSet;

  #[test]
  fn tab_id_new_generates_unique_ids() {
    let _lock = TAB_ID_TEST_LOCK.lock().unwrap();
    let mut ids = HashSet::new();
    for _ in 0..1024 {
      assert!(ids.insert(TabId::new()));
    }
  }

  #[test]
  fn tab_id_new_does_not_panic_on_counter_wraparound() {
    let _lock = TAB_ID_TEST_LOCK.lock().unwrap();

    let prev = NEXT_TAB_ID.swap(u64::MAX - 1, Ordering::Relaxed);

    // This will allocate ids at the end of the range and then wrap. The allocator must not panic,
    // and must never return 0.
    let a = TabId::new().0;
    let b = TabId::new().0;
    let c = TabId::new().0;

    assert_eq!(a, u64::MAX - 1);
    assert_eq!(b, u64::MAX);
    assert_ne!(c, 0);

    NEXT_TAB_ID.store(prev, Ordering::Relaxed);
  }

  #[test]
  fn rendered_frame_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<RenderedFrame>();
  }

  #[test]
  fn ui_to_worker_new_history_actions_are_debug_constructible() {
    let tab_id = TabId(1);

    // Ensure the new variants can be constructed and formatted. This is
    // intentionally lightweight: we just want compilation coverage for the
    // message protocol.
    let msgs = [
      UiToWorker::GoBack { tab_id },
      UiToWorker::GoForward { tab_id },
      UiToWorker::Reload { tab_id },
    ];

    for msg in msgs {
      let formatted = format!("{msg:?}");
      assert!(!formatted.is_empty());
    }
  }

  #[test]
  fn find_in_page_messages_are_debug_constructible() {
    let tab_id = TabId(1);
    let query = "test".to_string();

    // Ensure the new variants can be constructed and formatted. This is intentionally lightweight:
    // we just want compilation coverage for the message protocol.
    let ui_msgs = [
      UiToWorker::FindQuery {
        tab_id,
        query: query.clone(),
        case_sensitive: false,
      },
      UiToWorker::FindNext { tab_id },
      UiToWorker::FindPrev { tab_id },
      UiToWorker::FindStop { tab_id },
    ];

    for msg in ui_msgs {
      let formatted = format!("{msg:?}");
      assert!(!formatted.is_empty());
    }

    let worker_msg = WorkerToUi::FindResult {
      tab_id,
      query,
      case_sensitive: false,
      match_count: 3,
      active_match_index: Some(1),
    };
    let formatted = format!("{worker_msg:?}");
    assert!(!formatted.is_empty());
  }
}
