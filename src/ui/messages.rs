use crate::render_control::StageHeartbeat;
use crate::scroll::ScrollState;
use std::sync::atomic::{AtomicU64, Ordering};

pub use crate::interaction::KeyAction;
use tiny_skia::Pixmap;

static NEXT_TAB_ID: AtomicU64 = AtomicU64::new(1);

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
    let id = NEXT_TAB_ID
      .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |cur| {
        cur.checked_add(1)
      })
      .unwrap_or_else(|_| panic!("tab id counter overflowed"));
    Self(id)
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
  },
  CloseTab {
    tab_id: TabId,
  },
  SetActiveTab {
    tab_id: TabId,
  },
  Navigate {
    tab_id: TabId,
    url: String,
    reason: NavigationReason,
  },
  ViewportChanged {
    tab_id: TabId,
    viewport_css: (u32, u32),
    dpr: f32,
  },
  Scroll {
    tab_id: TabId,
    delta_css: (f32, f32),
    pointer_css: Option<(f32, f32)>,
  },
  PointerMove {
    tab_id: TabId,
    pos_css: (f32, f32),
    button: PointerButton,
  },
  PointerDown {
    tab_id: TabId,
    pos_css: (f32, f32),
    button: PointerButton,
  },
  PointerUp {
    tab_id: TabId,
    pos_css: (f32, f32),
    button: PointerButton,
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
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::collections::HashSet;

  #[test]
  fn tab_id_new_generates_unique_ids() {
    let mut ids = HashSet::new();
    for _ in 0..1024 {
      assert!(ids.insert(TabId::new()));
    }
  }

  #[test]
  fn rendered_frame_is_send() {
    fn assert_send<T: Send>() {}
    assert_send::<RenderedFrame>();
  }
}
