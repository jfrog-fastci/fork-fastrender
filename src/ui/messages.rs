use crate::render_control::StageHeartbeat;
use crate::scroll::ScrollState;
use crate::tree::box_tree::SelectControl;
use crate::ui::cancel::CancelGens;
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

/// An owned rendered frame produced by the render worker.
///
/// This owns the underlying pixel buffer (`tiny_skia::Pixmap`) and is expected to be sent to the
/// UI thread by move (avoid cloning large pixmaps).
pub struct RenderedFrame {
  pub pixmap: Pixmap,
  pub viewport_css: (u32, u32),
  pub dpr: f32,
  pub scroll_state: ScrollState,
}

impl std::fmt::Debug for RenderedFrame {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.debug_struct("RenderedFrame")
      .field("pixmap_px", &(self.pixmap.width(), self.pixmap.height()))
      .field("viewport_css", &self.viewport_css)
      .field("dpr", &self.dpr)
      .field("scroll_state", &self.scroll_state)
      .finish()
  }
}

/// Messages sent from the UI thread to the render worker.
#[derive(Debug)]
pub enum UiToWorker {
  CreateTab {
    tab_id: TabId,
    initial_url: Option<String>,
    cancel: CancelGens,
  },
  /// Optional alias for [`UiToWorker::CreateTab`].
  ///
  /// Kept for protocol flexibility as the browser UI evolves.
  NewTab {
    tab_id: TabId,
    initial_url: Option<String>,
  },
  CloseTab {
    tab_id: TabId,
  },
  SetActiveTab {
    tab_id: TabId,
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
  },
  /// User chose an option in a dropdown `<select>` popup.
  ///
  /// The UI should send this after receiving [`WorkerToUi::SelectDropdownOpened`].
  SelectDropdownChoose {
    tab_id: TabId,
    select_node_id: usize,
    option_node_id: usize,
  },
  TextInput {
    tab_id: TabId,
    text: String,
  },
  KeyAction {
    tab_id: TabId,
    key: KeyAction,
  },
  RequestRepaint {
    tab_id: TabId,
    reason: RepaintReason,
  },
}

/// Messages sent from the render worker to the UI thread.
#[derive(Debug)]
pub enum WorkerToUi {
  /// Coarse-grained stage heartbeat emitted while preparing or painting a document.
  Stage {
    tab_id: TabId,
    stage: StageHeartbeat,
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
  },
  ScrollStateUpdated {
    tab_id: TabId,
    scroll: ScrollState,
  },
  LoadingState {
    tab_id: TabId,
    loading: bool,
  },
  DebugLog {
    tab_id: TabId,
    line: String,
  },
  /// A dropdown `<select>` was clicked and should open a UI popup.
  ///
  /// `anchor_css` is in **viewport-local CSS pixels** so the UI can position the popup relative to
  /// the rendered frame.
  SelectDropdownOpened {
    tab_id: TabId,
    select_node_id: usize,
    control: crate::tree::box_tree::SelectControl,
    anchor_css: crate::geometry::Rect,
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
}
