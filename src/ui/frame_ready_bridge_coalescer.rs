use crate::ui::{RenderedFrame, TabId};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

/// Coalesce `WorkerToUi::FrameReady` pixmaps before they enter the UI message queue.
///
/// `RenderedFrame::pixmap` can be multi-megabyte; if the UI thread is busy (GPU upload/layout/etc),
/// queueing every `FrameReady` message can retain an unbounded number of pixmaps at once.
///
/// This structure bounds memory by storing only the latest frame per tab.
#[derive(Clone)]
pub struct FrameReadyBridgeCoalescer {
  latest_by_tab: Arc<Mutex<HashMap<TabId, RenderedFrame>>>,
}

impl FrameReadyBridgeCoalescer {
  pub fn new() -> Self {
    Self {
      latest_by_tab: Arc::new(Mutex::new(HashMap::new())),
    }
  }

  pub fn insert(&self, tab_id: TabId, frame: RenderedFrame) {
    // Drop the old pixmap outside the lock so the bridge thread spends minimal time holding it.
    let previous = { self.latest_by_tab.lock().insert(tab_id, frame) };
    drop(previous);
  }

  pub fn remove_tab(&self, tab_id: TabId) {
    let removed = { self.latest_by_tab.lock().remove(&tab_id) };
    drop(removed);
  }

  pub fn is_empty(&self) -> bool {
    self.latest_by_tab.lock().is_empty()
  }

  pub fn drain(&self) -> std::collections::hash_map::IntoIter<TabId, RenderedFrame> {
    let drained = std::mem::take(&mut *self.latest_by_tab.lock());
    drained.into_iter()
  }
}

#[cfg(test)]
mod tests {
  use super::FrameReadyBridgeCoalescer;
  use crate::scroll::{ScrollBounds, ScrollState};
  use crate::ui::messages::ScrollMetrics;
  use crate::ui::{RenderedFrame, TabId};
  use std::sync::atomic::{AtomicBool, Ordering};
  use std::sync::Arc;

  fn make_frame() -> RenderedFrame {
    RenderedFrame {
      pixmap: tiny_skia::Pixmap::new(2, 2).unwrap(),
      viewport_css: (2, 2),
      dpr: 1.0,
      scroll_state: ScrollState::default(),
      scroll_metrics: ScrollMetrics {
        viewport_css: (2, 2),
        scroll_css: (0.0, 0.0),
        bounds_css: ScrollBounds {
          min_x: 0.0,
          min_y: 0.0,
          max_x: 0.0,
          max_y: 0.0,
        },
        content_css: (2.0, 2.0),
      },
      next_tick: None,
    }
  }

  #[test]
  fn holds_at_most_one_frame_per_tab() {
    let coalescer = FrameReadyBridgeCoalescer::new();
    let tab_id = TabId(1);

    let running = Arc::new(AtomicBool::new(true));
    let running_for_thread = Arc::clone(&running);
    let producer = {
      let coalescer = coalescer.clone();
      std::thread::spawn(move || {
        for _ in 0..10_000 {
          coalescer.insert(tab_id, make_frame());
          assert!(
            coalescer.latest_by_tab.lock().len() <= 1,
            "expected at most 1 frame stored for a single tab"
          );
        }
        running_for_thread.store(false, Ordering::Relaxed);
      })
    };

    while running.load(Ordering::Relaxed) {
      // Drain periodically to simulate the UI thread catching up.
      let drained: Vec<_> = coalescer.drain().collect();
      assert!(drained.len() <= 1);
      assert!(coalescer.latest_by_tab.lock().len() <= 1);
      std::thread::yield_now();
    }

    producer.join().unwrap();

    let drained: Vec<_> = coalescer.drain().collect();
    assert!(drained.len() <= 1);
    assert!(coalescer.latest_by_tab.lock().len() <= 1);

    // Simulate tab close cleanup: pending frames for a closed tab should be dropped eagerly so the
    // bridge does not retain pixmaps forever once a tab is gone.
    coalescer.insert(tab_id, make_frame());
    assert_eq!(coalescer.latest_by_tab.lock().len(), 1);
    coalescer.remove_tab(tab_id);
    assert_eq!(coalescer.latest_by_tab.lock().len(), 0);
  }
}
