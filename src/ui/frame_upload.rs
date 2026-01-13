use super::browser_app::FrameReadyUpdate;
use super::messages::TabId;
use lru::LruCache;

/// Coalesces `WorkerToUi::FrameReady` pixmaps into at most one pending upload per tab.
///
/// The render worker can produce multiple frames before the windowed UI gets a chance to redraw.
/// Uploading every intermediate pixmap into a GPU texture is expensive, so we keep only the most
/// recent frame per tab and drop the rest.
///
/// The windowed browser UI relies on this to keep at least the latest frame for background tabs so
/// switching tabs can display immediately (no "Waiting for first frame…"). Call [`Self::remove_tab`]
/// when a tab is closed so stale pixmaps do not accumulate.
#[derive(Debug)]
pub struct FrameUploadCoalescer {
  latest_by_tab: LruCache<TabId, FrameReadyUpdate>,
  total_estimated_bytes: u64,
}

impl Default for FrameUploadCoalescer {
  fn default() -> Self {
    Self::new()
  }
}

impl FrameUploadCoalescer {
  pub fn new() -> Self {
    Self {
      latest_by_tab: LruCache::unbounded(),
      total_estimated_bytes: 0,
    }
  }

  /// Returns the estimated byte size of a pending `FrameReady` pixmap.
  ///
  /// This is intentionally cheap: tiny-skia pixmaps are stored as tightly-packed RGBA8.
  pub fn estimated_bytes_for(frame: &FrameReadyUpdate) -> u64 {
    u64::from(frame.pixmap.width())
      .saturating_mul(u64::from(frame.pixmap.height()))
      .saturating_mul(4)
  }

  /// Total estimated bytes for all pending pixmaps currently stored in this coalescer.
  pub fn total_estimated_bytes(&self) -> u64 {
    self.total_estimated_bytes
  }

  pub fn is_empty(&self) -> bool {
    self.latest_by_tab.is_empty()
  }

  pub fn has_pending_for_tab(&self, tab_id: TabId) -> bool {
    self.latest_by_tab.peek(&tab_id).is_some()
  }

  pub fn remove_tab(&mut self, tab_id: TabId) {
    if let Some(frame) = self.latest_by_tab.pop(&tab_id) {
      self.total_estimated_bytes = self
        .total_estimated_bytes
        .saturating_sub(Self::estimated_bytes_for(&frame));
    }
  }

  pub fn clear(&mut self) {
    // `lru` versions vary in whether `clear()` is implemented on `LruCache`, so reset by
    // reinitializing the internal map.
    self.latest_by_tab = LruCache::unbounded();
    self.total_estimated_bytes = 0;
  }

  /// Store `frame`, coalescing with any already-pending upload for the same tab.
  pub fn push(&mut self, frame: FrameReadyUpdate) {
    let bytes = Self::estimated_bytes_for(&frame);
    // Overwrite older pending frames for this tab (dropping the pixmap without uploading).
    if let Some(prev) = self.latest_by_tab.put(frame.tab_id, frame) {
      self.total_estimated_bytes = self
        .total_estimated_bytes
        .saturating_sub(Self::estimated_bytes_for(&prev));
    }
    self.total_estimated_bytes = self.total_estimated_bytes.saturating_add(bytes);
  }

  /// Evict pending uploads until the total estimated size is at most `budget_bytes`.
  ///
  /// The eviction order is "least recently updated" first (LRU). The `preserve` tab, if supplied,
  /// will never be evicted.
  pub fn evict_to_budget(&mut self, budget_bytes: u64, preserve: Option<TabId>) -> usize {
    if self.total_estimated_bytes <= budget_bytes {
      return 0;
    }

    let mut evicted = 0usize;
    let mut preserved: Option<(TabId, FrameReadyUpdate)> = None;

    while self.total_estimated_bytes > budget_bytes {
      if self.latest_by_tab.is_empty() {
        break;
      }

      if let Some(preserve_id) = preserve {
        if self.latest_by_tab.len() == 1 && self.latest_by_tab.peek(&preserve_id).is_some() {
          break;
        }
      }

      let Some((tab_id, frame)) = self.latest_by_tab.pop_lru() else {
        break;
      };

      if preserve == Some(tab_id) {
        // Preserve this entry by stashing it until we're done evicting. Keep the byte estimate
        // unchanged because we still hold the pixmap in memory and intend to keep it.
        preserved = Some((tab_id, frame));
        continue;
      }

      self.total_estimated_bytes = self
        .total_estimated_bytes
        .saturating_sub(Self::estimated_bytes_for(&frame));
      evicted += 1;
    }

    if let Some((tab_id, frame)) = preserved {
      // Put the preserved frame back. We never subtracted its bytes, so don't add them here.
      let replaced = self.latest_by_tab.put(tab_id, frame);
      if let Some(replaced) = replaced {
        // Shouldn't happen, but keep accounting correct if it does (e.g. if a caller preserved a
        // tab that was reinserted during eviction).
        self.total_estimated_bytes = self
          .total_estimated_bytes
          .saturating_sub(Self::estimated_bytes_for(&replaced));
      }
    }

    evicted
  }

  /// Remove and return the pending upload for `tab_id`, if any.
  pub fn take_for_tab(&mut self, tab_id: TabId) -> Option<FrameReadyUpdate> {
    let frame = self.latest_by_tab.pop(&tab_id)?;
    self.total_estimated_bytes = self
      .total_estimated_bytes
      .saturating_sub(Self::estimated_bytes_for(&frame));
    Some(frame)
  }

  /// Drains all pending uploads.
  pub fn drain(&mut self) -> impl Iterator<Item = FrameReadyUpdate> {
    self.total_estimated_bytes = 0;
    // `lru` versions vary in whether `clear()` is implemented on `LruCache`, so drain by replacing.
    std::mem::replace(&mut self.latest_by_tab, LruCache::unbounded())
      .into_iter()
      .map(|(_tab_id, frame)| frame)
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

  #[test]
  fn take_for_tab_removes_only_selected_tab() {
    let tab_a = TabId(1);
    let tab_b = TabId(2);
    let mut coalescer = FrameUploadCoalescer::new();

    coalescer.push(make_frame(tab_a, (1, 1), (10, 10), 1.0));
    coalescer.push(make_frame(tab_b, (2, 2), (20, 20), 1.0));

    let taken = coalescer.take_for_tab(tab_a).expect("tab A should be pending");
    assert_eq!(taken.tab_id, tab_a);
    assert!(!coalescer.has_pending_for_tab(tab_a));
    assert!(coalescer.has_pending_for_tab(tab_b));

    let drained: Vec<_> = coalescer.drain().collect();
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].tab_id, tab_b);
  }

  #[test]
  fn take_for_tab_returns_latest_frame_for_that_tab() {
    let tab = TabId(1);
    let mut coalescer = FrameUploadCoalescer::new();

    coalescer.push(make_frame(tab, (1, 1), (100, 100), 1.0));
    coalescer.push(make_frame(tab, (3, 4), (200, 150), 2.0));

    let taken = coalescer.take_for_tab(tab).expect("frame should be pending");
    assert_eq!(taken.tab_id, tab);
    assert_eq!((taken.pixmap.width(), taken.pixmap.height()), (3, 4));
    assert_eq!(taken.viewport_css, (200, 150));
    assert!((taken.dpr - 2.0).abs() < f32::EPSILON);

    assert!(coalescer.is_empty());
  }

  #[test]
  fn accounting_overwrites_frame_for_same_tab() {
    let tab = TabId(1);
    let mut coalescer = FrameUploadCoalescer::new();

    let frame_a = make_frame(tab, (1, 1), (100, 100), 1.0);
    let bytes_a = FrameUploadCoalescer::estimated_bytes_for(&frame_a);
    coalescer.push(frame_a);
    assert_eq!(coalescer.total_estimated_bytes(), bytes_a);

    let frame_b = make_frame(tab, (2, 3), (200, 150), 2.0);
    let bytes_b = FrameUploadCoalescer::estimated_bytes_for(&frame_b);
    coalescer.push(frame_b);
    assert_eq!(
      coalescer.total_estimated_bytes(),
      bytes_b,
      "expected overwrite to replace byte accounting"
    );
  }

  #[test]
  fn eviction_respects_preserved_tab() {
    let preserved = TabId(1);
    let tab_a = TabId(2);
    let tab_b = TabId(3);
    let mut coalescer = FrameUploadCoalescer::new();

    let preserve_frame = make_frame(preserved, (1, 1), (10, 10), 1.0);
    let preserve_bytes = FrameUploadCoalescer::estimated_bytes_for(&preserve_frame);
    coalescer.push(preserve_frame);

    // Add two large pending frames so eviction is required.
    coalescer.push(make_frame(tab_a, (256, 256), (10, 10), 1.0));
    coalescer.push(make_frame(tab_b, (256, 256), (10, 10), 1.0));

    let evicted = coalescer.evict_to_budget(preserve_bytes, Some(preserved));
    assert!(evicted >= 1, "expected at least one tab to be evicted");
    assert!(coalescer.has_pending_for_tab(preserved));
    assert_eq!(coalescer.total_estimated_bytes(), preserve_bytes);
  }
}
