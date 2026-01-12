use super::browser_app::FrameReadyUpdate;
use super::messages::TabId;
use std::collections::HashMap;

/// Coalesces `WorkerToUi::FrameReady` pixmaps into at most one pending upload per tab.
///
/// The render worker can produce multiple frames before the windowed UI gets a chance to redraw.
/// Uploading every intermediate pixmap into a GPU texture is expensive, so we keep only the most
/// recent frame per tab and drop the rest.
///
/// The windowed browser UI relies on this to keep at least the latest frame for background tabs so
/// switching tabs can display immediately (no "Waiting for first frame…"). Call [`Self::remove_tab`]
/// when a tab is closed so stale pixmaps do not accumulate.
#[derive(Debug, Default)]
pub struct FrameUploadCoalescer {
  latest_by_tab: HashMap<TabId, FrameReadyUpdate>,
}

impl FrameUploadCoalescer {
  pub fn new() -> Self {
    Self {
      latest_by_tab: HashMap::new(),
    }
  }

  pub fn is_empty(&self) -> bool {
    self.latest_by_tab.is_empty()
  }

  pub fn has_pending_for_tab(&self, tab_id: TabId) -> bool {
    self.latest_by_tab.contains_key(&tab_id)
  }

  pub fn remove_tab(&mut self, tab_id: TabId) {
    self.latest_by_tab.remove(&tab_id);
  }

  pub fn clear(&mut self) {
    self.latest_by_tab.clear();
  }

  /// Store `frame`, coalescing with any already-pending upload for the same tab.
  pub fn push(&mut self, frame: FrameReadyUpdate) {
    // Overwrite older pending frames for this tab (dropping the pixmap without uploading).
    self.latest_by_tab.insert(frame.tab_id, frame);
  }

  /// Drains all pending uploads.
  pub fn drain(&mut self) -> std::collections::hash_map::IntoValues<TabId, FrameReadyUpdate> {
    std::mem::take(&mut self.latest_by_tab).into_values()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::collections::HashSet;

  fn make_frame(
    tab_id: TabId,
    pixmap_px: (u32, u32),
    viewport_css: (u32, u32),
    dpr: f32,
  ) -> FrameReadyUpdate {
    FrameReadyUpdate {
      tab_id,
      pixmap: tiny_skia::Pixmap::new(pixmap_px.0, pixmap_px.1).unwrap(),
      viewport_css,
      dpr,
    }
  }

  #[test]
  fn coalesces_multiple_frames_for_same_tab() {
    let tab = TabId(1);
    let mut coalescer = FrameUploadCoalescer::new();

    coalescer.push(make_frame(tab, (1, 1), (100, 100), 1.0));
    coalescer.push(make_frame(tab, (2, 3), (200, 150), 2.0));

    let mut drained: Vec<_> = coalescer.drain().collect();
    assert_eq!(drained.len(), 1);
    let frame = drained.pop().unwrap();
    assert_eq!(frame.tab_id, tab);
    assert_eq!((frame.pixmap.width(), frame.pixmap.height()), (2, 3));
    assert_eq!(frame.viewport_css, (200, 150));
    assert!((frame.dpr - 2.0).abs() < f32::EPSILON);
  }

  #[test]
  fn stores_frames_for_multiple_tabs() {
    let tab_a = TabId(1);
    let tab_b = TabId(2);
    let mut coalescer = FrameUploadCoalescer::new();

    coalescer.push(make_frame(tab_a, (1, 1), (10, 10), 1.0));
    coalescer.push(make_frame(tab_b, (2, 2), (20, 20), 1.0));

    assert!(!coalescer.is_empty());
    assert!(coalescer.has_pending_for_tab(tab_a));
    assert!(coalescer.has_pending_for_tab(tab_b));

    let drained: Vec<_> = coalescer.drain().collect();
    assert_eq!(drained.len(), 2);
    let ids: HashSet<_> = drained.into_iter().map(|f| f.tab_id).collect();
    assert_eq!(ids, HashSet::from([tab_a, tab_b]));
  }
}
