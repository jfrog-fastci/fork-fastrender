use super::browser_app::FrameReadyUpdate;
use super::messages::TabId;
use lru::LruCache;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameUploadCoalescerStats {
  /// Number of times [`FrameUploadCoalescer::push`] was called.
  pub push_calls: u64,
  /// Number of times a pending frame for a tab was overwritten (coalesced / dropped without
  /// uploading).
  pub overwritten_frames: u64,
  /// Number of frames removed from the pending set via [`FrameUploadCoalescer::take`] or
  /// [`FrameUploadCoalescer::drain`].
  pub drained_frames: u64,
  /// Current pending frame count (one per tab).
  pub pending_tabs: usize,
  /// Max pending frame count (one per tab) observed since the last [`FrameUploadCoalescer::take_stats`].
  pub max_pending_tabs: usize,
}

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

  received_total: u64,
  dropped_total: u64,
  drained_total: u64,

  push_calls: u64,
  overwritten_frames: u64,
  drained_frames: u64,
  max_pending_tabs: usize,

  /// Total number of frames dropped due to coalescing/eviction (cumulative across the run).
  dropped_frames_total: u64,
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
      received_total: 0,
      dropped_total: 0,
      drained_total: 0,
      push_calls: 0,
      overwritten_frames: 0,
      drained_frames: 0,
      max_pending_tabs: 0,
      dropped_frames_total: 0,
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

  /// Total number of frames received by this coalescer (number of [`Self::push`] calls).
  pub fn received_total(&self) -> u64 {
    self.received_total
  }

  /// Total number of frames dropped/coalesced because a newer frame arrived for the same tab
  /// before the older frame could be drained and uploaded.
  pub fn dropped_total(&self) -> u64 {
    self.dropped_total
  }

  /// Total number of frames drained (removed from the coalescer via [`Self::take`] or
  /// [`Self::drain`]).
  pub fn drained_total(&self) -> u64 {
    self.drained_total
  }

  pub fn is_empty(&self) -> bool {
    self.latest_by_tab.is_empty()
  }

  pub fn pending_tab_count(&self) -> usize {
    self.latest_by_tab.len()
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
    self.received_total = self.received_total.saturating_add(1);
    self.push_calls = self.push_calls.saturating_add(1);

    let bytes = Self::estimated_bytes_for(&frame);
    // Overwrite older pending frames for this tab (dropping the pixmap without uploading).
    if let Some(prev) = self.latest_by_tab.put(frame.tab_id, frame) {
      self.dropped_total = self.dropped_total.saturating_add(1);
      self.overwritten_frames = self.overwritten_frames.saturating_add(1);
      self.dropped_frames_total = self.dropped_frames_total.saturating_add(1);
      self.total_estimated_bytes = self
        .total_estimated_bytes
        .saturating_sub(Self::estimated_bytes_for(&prev));
    }
    self.total_estimated_bytes = self.total_estimated_bytes.saturating_add(bytes);
    self.max_pending_tabs = self.max_pending_tabs.max(self.latest_by_tab.len());
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
      self.dropped_frames_total = self.dropped_frames_total.saturating_add(1);
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

  /// Takes and removes the latest pending upload for `tab_id`, if any.
  pub fn take(&mut self, tab_id: TabId) -> Option<FrameReadyUpdate> {
    let frame = self.latest_by_tab.pop(&tab_id)?;
    self.total_estimated_bytes = self
      .total_estimated_bytes
      .saturating_sub(Self::estimated_bytes_for(&frame));
    self.drained_total = self.drained_total.saturating_add(1);
    self.drained_frames = self.drained_frames.saturating_add(1);
    Some(frame)
  }

  /// Drains all pending uploads.
  pub fn drain(&mut self) -> impl Iterator<Item = FrameReadyUpdate> {
    let drained = self.latest_by_tab.len() as u64;
    self.drained_total = self.drained_total.saturating_add(drained);
    self.drained_frames = self.drained_frames.saturating_add(drained);
    self.total_estimated_bytes = 0;
    // `lru` versions vary in whether `clear()` is implemented on `LruCache`, so drain by replacing.
    std::mem::replace(&mut self.latest_by_tab, LruCache::unbounded())
      .into_iter()
      .map(|(_tab_id, frame)| frame)
  }

  /// Returns coalescing counters accumulated since the last call and resets them.
  ///
  /// Intended to be called once per UI frame so callers can report per-frame deltas (e.g. HUD).
  pub fn take_stats(&mut self) -> FrameUploadCoalescerStats {
    let pending_tabs = self.latest_by_tab.len();
    let max_pending_tabs = self.max_pending_tabs.max(pending_tabs);
    let stats = FrameUploadCoalescerStats {
      push_calls: self.push_calls,
      overwritten_frames: self.overwritten_frames,
      drained_frames: self.drained_frames,
      pending_tabs,
      max_pending_tabs,
    };

    self.push_calls = 0;
    self.overwritten_frames = 0;
    self.drained_frames = 0;
    // Baseline the next window so it accounts for already-pending frames even if no new pushes
    // arrive before the next `take_stats` call.
    self.max_pending_tabs = pending_tabs;

    stats
  }

  /// Total number of frames dropped due to coalescing and eviction since this coalescer was created.
  pub fn dropped_frames(&self) -> u64 {
    self.dropped_frames_total
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
  fn take_returns_latest_for_tab_and_removes_it() {
    let tab = TabId(1);
    let mut coalescer = FrameUploadCoalescer::new();

    coalescer.push(make_frame(tab, (3, 4), (100, 80), 1.5));
    assert!(coalescer.has_pending_for_tab(tab));

    let frame = coalescer.take(tab).expect("expected pending frame");
    assert_eq!(frame.tab_id, tab);
    assert_eq!((frame.pixmap.width(), frame.pixmap.height()), (3, 4));
    assert_eq!(frame.viewport_css, (100, 80));
    assert!((frame.dpr - 1.5).abs() < f32::EPSILON);

    assert!(!coalescer.has_pending_for_tab(tab));
    assert!(coalescer.take(tab).is_none());
  }

  #[test]
  fn take_only_affects_requested_tab() {
    let tab_a = TabId(1);
    let tab_b = TabId(2);
    let mut coalescer = FrameUploadCoalescer::new();

    coalescer.push(make_frame(tab_a, (1, 1), (10, 10), 1.0));
    coalescer.push(make_frame(tab_b, (2, 2), (20, 20), 1.0));

    let _ = coalescer.take(tab_a).expect("expected tab_a frame");
    assert!(!coalescer.has_pending_for_tab(tab_a));
    assert!(coalescer.has_pending_for_tab(tab_b));

    let frame_b = coalescer.take(tab_b).expect("expected tab_b frame");
    assert_eq!(frame_b.tab_id, tab_b);
  }

  #[test]
  fn take_yields_only_newest_frame_for_tab() {
    let tab = TabId(1);
    let mut coalescer = FrameUploadCoalescer::new();

    coalescer.push(make_frame(tab, (1, 1), (10, 10), 1.0));
    coalescer.push(make_frame(tab, (9, 7), (90, 70), 2.0));

    let frame = coalescer.take(tab).expect("expected pending frame");
    assert_eq!((frame.pixmap.width(), frame.pixmap.height()), (9, 7));
    assert_eq!(frame.viewport_css, (90, 70));
    assert!((frame.dpr - 2.0).abs() < f32::EPSILON);
    assert!(coalescer.take(tab).is_none());
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

  #[test]
  fn counters_track_overwrites() {
    let tab = TabId(1);
    let mut coalescer = FrameUploadCoalescer::new();

    coalescer.push(make_frame(tab, (1, 1), (10, 10), 1.0));
    coalescer.push(make_frame(tab, (2, 2), (10, 10), 1.0));

    let stats = coalescer.take_stats();
    assert_eq!(stats.push_calls, 2);
    assert_eq!(stats.overwritten_frames, 1);
    assert_eq!(stats.pending_tabs, 1);

    assert_eq!(coalescer.received_total(), 2);
    assert_eq!(coalescer.dropped_total(), 1);
    assert_eq!(coalescer.drained_total(), 0);
  }

  #[test]
  fn counters_track_drains_and_pending_count() {
    let tab_a = TabId(1);
    let tab_b = TabId(2);
    let mut coalescer = FrameUploadCoalescer::new();

    coalescer.push(make_frame(tab_a, (1, 1), (10, 10), 1.0));
    coalescer.push(make_frame(tab_b, (1, 1), (10, 10), 1.0));
    assert_eq!(coalescer.pending_tab_count(), 2);

    let drained: Vec<_> = coalescer.drain().collect();
    assert_eq!(drained.len(), 2);
    assert_eq!(coalescer.pending_tab_count(), 0);

    let stats = coalescer.take_stats();
    assert_eq!(stats.drained_frames, 2);
    assert_eq!(stats.pending_tabs, 0);

    assert_eq!(coalescer.received_total(), 2);
    assert_eq!(coalescer.dropped_total(), 0);
    assert_eq!(coalescer.drained_total(), 2);
  }

  #[test]
  fn drained_total_counts_take() {
    let tab = TabId(1);
    let mut coalescer = FrameUploadCoalescer::new();

    coalescer.push(make_frame(tab, (1, 1), (10, 10), 1.0));
    assert_eq!(coalescer.drained_total(), 0);
    let _ = coalescer.take(tab).expect("expected pending frame");
    assert_eq!(coalescer.drained_total(), 1);
  }
}
