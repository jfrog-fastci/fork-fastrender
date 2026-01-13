use super::messages::{RepaintReason, TabId, UiToWorker};
use super::PointerButton;
use super::PointerModifiers;
use std::collections::HashMap;
use std::time::Duration;

// When the router coalesces multiple `Tick` messages into one, sum their deltas but clamp to a
// reasonable upper bound to avoid pathological "catch-up" work after long stalls.
const MAX_COALESCED_TICK_DELTA: Duration = Duration::from_secs(1);

/// Coalesce high-frequency `UiToWorker` messages *before* they enter the worker runtime queue.
///
/// The render worker has a dedicated router thread (`render_worker::spawn_worker_with_factory_inner`)
/// that forwards UI messages to the main worker runtime thread. The runtime channel is an unbounded
/// `std::sync::mpsc` queue, so bursts (pointer move, viewport changes, scroll wheel, IME updates)
/// can otherwise grow without bound while the runtime thread is busy preparing/painting.
///
/// This helper keeps only the "latest relevant" state per tab for selected message kinds and
/// supports barrier semantics: any non-coalescible message flushes all pending coalesced messages
/// (in deterministic order) before forwarding the barrier.
#[derive(Debug, Default)]
pub(crate) struct UiToWorkerRouterCoalescer {
  next_seq: u64,
  tabs: HashMap<TabId, TabPending>,
}

#[derive(Debug, Default)]
struct TabPending {
  viewport_changed: Option<Pending<ViewportChanged>>,
  scroll_to: Option<Pending<(f32, f32)>>,
  scroll: Option<PendingScroll>,
  pointer_move: Option<Pending<PointerMove>>,
  tick: Option<Pending<Duration>>,
  text_input: Option<Pending<String>>,
  ime_preedit: Option<Pending<ImePreedit>>,
  find_query: Option<Pending<FindQuery>>,
  request_repaint: Option<Pending<RepaintReason>>,
}

impl TabPending {
  fn has_pending(&self) -> bool {
    self.viewport_changed.is_some()
      || self.scroll_to.is_some()
      || self.scroll.is_some()
      || self.pointer_move.is_some()
      || self.tick.is_some()
      || self.text_input.is_some()
      || self.ime_preedit.is_some()
      || self.find_query.is_some()
      || self.request_repaint.is_some()
  }
}

#[derive(Debug, Clone)]
struct Pending<T> {
  seq: u64,
  value: T,
}

#[derive(Debug, Clone)]
struct ViewportChanged {
  viewport_css: (u32, u32),
  dpr: f32,
}

#[derive(Debug, Clone)]
struct PointerMove {
  pos_css: (f32, f32),
  button: PointerButton,
  modifiers: PointerModifiers,
}

#[derive(Debug, Clone)]
struct ImePreedit {
  text: String,
  cursor: Option<(usize, usize)>,
}

#[derive(Debug, Clone)]
struct FindQuery {
  query: String,
  case_sensitive: bool,
}

#[derive(Debug, Clone)]
struct PendingScroll {
  seq: u64,
  delta_css: (f32, f32),
  pointer_css: Option<(f32, f32)>,
  pointer_key: Option<(i32, i32)>,
}

impl UiToWorkerRouterCoalescer {
  pub(crate) fn new() -> Self {
    Self::default()
  }

  pub(crate) fn has_pending(&self) -> bool {
    self.tabs.values().any(|tab| tab.has_pending())
  }

  fn next_seq(&mut self) -> u64 {
    let seq = self.next_seq;
    self.next_seq = self.next_seq.wrapping_add(1);
    seq
  }

  fn tab_mut(&mut self, tab_id: TabId) -> &mut TabPending {
    self.tabs.entry(tab_id).or_default()
  }

  /// Push a new message into the coalescer.
  ///
  /// Returns any messages that should be forwarded to the runtime immediately (including a flush of
  /// pending coalesced state for barrier messages).
  pub(crate) fn push(&mut self, msg: UiToWorker) -> Vec<UiToWorker> {
    match msg {
      UiToWorker::PointerMove {
        tab_id,
        pos_css,
        button,
        modifiers,
      } => {
        let seq = self.next_seq();
        self.tab_mut(tab_id).pointer_move = Some(Pending {
          seq,
          value: PointerMove {
            pos_css,
            button,
            modifiers,
          },
        });
        Vec::new()
      }
      UiToWorker::ViewportChanged {
        tab_id,
        viewport_css,
        dpr,
      } => {
        let seq = self.next_seq();
        self.tab_mut(tab_id).viewport_changed = Some(Pending {
          seq,
          value: ViewportChanged { viewport_css, dpr },
        });
        Vec::new()
      }
      UiToWorker::ScrollTo { tab_id, pos_css } => {
        let seq = self.next_seq();
        self.tab_mut(tab_id).scroll_to = Some(Pending { seq, value: pos_css });
        Vec::new()
      }
      UiToWorker::Scroll {
        tab_id,
        delta_css,
        pointer_css,
      } => self.push_scroll(tab_id, delta_css, pointer_css),
      UiToWorker::Tick { tab_id, delta } => {
        let seq = self.next_seq();
        let tab = self.tab_mut(tab_id);
        let delta = delta.min(MAX_COALESCED_TICK_DELTA);
        match tab.tick.as_mut() {
          Some(pending) => {
            pending.value = pending
              .value
              .checked_add(delta)
              .unwrap_or(MAX_COALESCED_TICK_DELTA);
            if pending.value > MAX_COALESCED_TICK_DELTA {
              pending.value = MAX_COALESCED_TICK_DELTA;
            }
            pending.seq = seq;
          }
          None => {
            tab.tick = Some(Pending { seq, value: delta });
          }
        }
        Vec::new()
      }
      UiToWorker::TextInput { tab_id, text } => {
        let seq = self.next_seq();
        let tab = self.tab_mut(tab_id);
        match tab.text_input.as_mut() {
          Some(pending) => {
            pending.value.push_str(&text);
            pending.seq = seq;
          }
          None => {
            tab.text_input = Some(Pending { seq, value: text });
          }
        }
        Vec::new()
      }
      UiToWorker::ImePreedit {
        tab_id,
        text,
        cursor,
      } => {
        let seq = self.next_seq();
        self.tab_mut(tab_id).ime_preedit = Some(Pending {
          seq,
          value: ImePreedit { text, cursor },
        });
        Vec::new()
      }
      UiToWorker::FindQuery {
        tab_id,
        query,
        case_sensitive,
      } => {
        let seq = self.next_seq();
        self.tab_mut(tab_id).find_query = Some(Pending {
          seq,
          value: FindQuery {
            query,
            case_sensitive,
          },
        });
        Vec::new()
      }
      UiToWorker::RequestRepaint { tab_id, reason } => {
        let seq = self.next_seq();
        self.tab_mut(tab_id).request_repaint = Some(Pending { seq, value: reason });
        Vec::new()
      }
      barrier => {
        let mut out = self.flush();
        out.push(barrier);
        out
      }
    }
  }

  fn push_scroll(
    &mut self,
    tab_id: TabId,
    delta_css: (f32, f32),
    pointer_css: Option<(f32, f32)>,
  ) -> Vec<UiToWorker> {
    // Mirror `BrowserRuntime::handle_message` semantics:
    // - Ignore invalid/no-op deltas.
    // - Treat non-finite deltas as 0.
    let mut dx = delta_css.0;
    let mut dy = delta_css.1;
    if (!dx.is_finite() && !dy.is_finite()) || (dx == 0.0 && dy == 0.0) {
      return Vec::new();
    }
    dx = if dx.is_finite() { dx } else { 0.0 };
    dy = if dy.is_finite() { dy } else { 0.0 };
    if dx == 0.0 && dy == 0.0 {
      return Vec::new();
    }

    let pointer_css = pointer_css.filter(|(x, y)| x.is_finite() && y.is_finite());
    let pointer_key = pointer_css.map(|(x, y)| (x.round() as i32, y.round() as i32));
    let seq = self.next_seq();

    let tab = self.tab_mut(tab_id);

    match tab.scroll.as_mut() {
      Some(pending) if pending.pointer_key == pointer_key => {
        // Safe case: when pointer position is stable, the worker's scroll targeting semantics
        // (element-under-pointer scrolling vs viewport scrolling) will be consistent, so summing
        // deltas preserves the user's scroll distance while bounding queue growth.
        pending.delta_css.0 += dx;
        pending.delta_css.1 += dy;
        pending.seq = seq;
        pending.pointer_css = pointer_css;
      }
      _ => {
        // Unsafe case: if the pointer position changes, the worker may target a different scroll
        // container. To avoid unbounded message growth while the runtime thread is busy, keep only
        // the latest scroll delta for this tab in the current coalescing window.
        tab.scroll = Some(PendingScroll {
          seq,
          delta_css: (dx, dy),
          pointer_css,
          pointer_key,
        });
      }
    }

    Vec::new()
  }

  /// Flush all pending coalesced state.
  pub(crate) fn flush(&mut self) -> Vec<UiToWorker> {
    let mut items: Vec<(u64, UiToWorker)> = Vec::new();

    for (&tab_id, tab) in self.tabs.iter_mut() {
      if let Some(pending) = tab.viewport_changed.take() {
        items.push((
          pending.seq,
          UiToWorker::ViewportChanged {
            tab_id,
            viewport_css: pending.value.viewport_css,
            dpr: pending.value.dpr,
          },
        ));
      }
      if let Some(pending) = tab.scroll_to.take() {
        items.push((
          pending.seq,
          UiToWorker::ScrollTo {
            tab_id,
            pos_css: pending.value,
          },
        ));
      }
      if let Some(pending) = tab.scroll.take() {
        items.push((
          pending.seq,
          UiToWorker::Scroll {
            tab_id,
            delta_css: pending.delta_css,
            pointer_css: pending.pointer_css,
          },
        ));
      }
      if let Some(pending) = tab.pointer_move.take() {
        items.push((
          pending.seq,
          UiToWorker::PointerMove {
            tab_id,
            pos_css: pending.value.pos_css,
            button: pending.value.button,
            modifiers: pending.value.modifiers,
          },
        ));
      }
      if let Some(pending) = tab.tick.take() {
        items.push((
          pending.seq,
          UiToWorker::Tick {
            tab_id,
            delta: pending.value,
          },
        ));
      }
      if let Some(pending) = tab.text_input.take() {
        items.push((
          pending.seq,
          UiToWorker::TextInput {
            tab_id,
            text: pending.value,
          },
        ));
      }
      if let Some(pending) = tab.ime_preedit.take() {
        items.push((
          pending.seq,
          UiToWorker::ImePreedit {
            tab_id,
            text: pending.value.text,
            cursor: pending.value.cursor,
          },
        ));
      }
      if let Some(pending) = tab.find_query.take() {
        items.push((
          pending.seq,
          UiToWorker::FindQuery {
            tab_id,
            query: pending.value.query,
            case_sensitive: pending.value.case_sensitive,
          },
        ));
      }
      if let Some(pending) = tab.request_repaint.take() {
        items.push((
          pending.seq,
          UiToWorker::RequestRepaint {
            tab_id,
            reason: pending.value,
          },
        ));
      }
    }

    // Drop empty per-tab state entries so closed tabs don't accumulate in the router.
    self.tabs.retain(|_, tab| tab.has_pending());

    items.sort_by_key(|(seq, _)| *seq);
    items.into_iter().map(|(_, msg)| msg).collect()
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::ui::messages::DownloadId;

  fn pm(tab_id: TabId, x: f32, y: f32) -> UiToWorker {
    UiToWorker::PointerMove {
      tab_id,
      pos_css: (x, y),
      button: PointerButton::None,
      modifiers: PointerModifiers::NONE,
    }
  }

  #[test]
  fn coalesces_pointer_moves_per_tab() {
    let mut c = UiToWorkerRouterCoalescer::new();
    assert!(c.push(pm(TabId(1), 1.0, 2.0)).is_empty());
    assert!(c.push(pm(TabId(1), 3.0, 4.0)).is_empty());

    let out = c.flush();
    assert_eq!(out.len(), 1);
    match &out[0] {
      UiToWorker::PointerMove { tab_id, pos_css, .. } => {
        assert_eq!(*tab_id, TabId(1));
        assert_eq!(*pos_css, (3.0, 4.0));
      }
      other => panic!("expected PointerMove, got {other:?}"),
    }
  }

  #[test]
  fn coalesces_viewport_changes() {
    let mut c = UiToWorkerRouterCoalescer::new();
    assert!(c
      .push(UiToWorker::ViewportChanged {
        tab_id: TabId(1),
        viewport_css: (100, 200),
        dpr: 1.0,
      })
      .is_empty());
    assert!(c
      .push(UiToWorker::ViewportChanged {
        tab_id: TabId(1),
        viewport_css: (300, 400),
        dpr: 2.0,
      })
      .is_empty());

    let out = c.flush();
    assert_eq!(out.len(), 1);
    match &out[0] {
      UiToWorker::ViewportChanged {
        tab_id,
        viewport_css,
        dpr,
      } => {
        assert_eq!(*tab_id, TabId(1));
        assert_eq!(*viewport_css, (300, 400));
        assert_eq!(*dpr, 2.0);
      }
      other => panic!("expected ViewportChanged, got {other:?}"),
    }
  }

  #[test]
  fn coalesces_scroll_to() {
    let mut c = UiToWorkerRouterCoalescer::new();
    assert!(c
      .push(UiToWorker::ScrollTo {
        tab_id: TabId(1),
        pos_css: (1.0, 2.0),
      })
      .is_empty());
    assert!(c
      .push(UiToWorker::ScrollTo {
        tab_id: TabId(1),
        pos_css: (3.0, 4.0),
      })
      .is_empty());

    let out = c.flush();
    assert_eq!(out.len(), 1);
    match &out[0] {
      UiToWorker::ScrollTo { tab_id, pos_css } => {
        assert_eq!(*tab_id, TabId(1));
        assert_eq!(*pos_css, (3.0, 4.0));
      }
      other => panic!("expected ScrollTo, got {other:?}"),
    }
  }

  #[test]
  fn sums_scroll_deltas_when_pointer_matches() {
    let mut c = UiToWorkerRouterCoalescer::new();
    assert!(c
      .push(UiToWorker::Scroll {
        tab_id: TabId(1),
        delta_css: (1.0, 2.0),
        pointer_css: Some((10.0, 10.0)),
      })
      .is_empty());
    assert!(c
      .push(UiToWorker::Scroll {
        tab_id: TabId(1),
        delta_css: (3.0, 4.0),
        pointer_css: Some((10.0, 10.0)),
      })
      .is_empty());

    let out = c.flush();
    assert_eq!(out.len(), 1);
    match &out[0] {
      UiToWorker::Scroll {
        tab_id,
        delta_css,
        pointer_css,
      } => {
        assert_eq!(*tab_id, TabId(1));
        assert_eq!(*delta_css, (4.0, 6.0));
        assert_eq!(*pointer_css, Some((10.0, 10.0)));
      }
      other => panic!("expected Scroll, got {other:?}"),
    }
  }

  #[test]
  fn keeps_latest_scroll_when_pointer_differs() {
    let mut c = UiToWorkerRouterCoalescer::new();
    assert!(c
      .push(UiToWorker::Scroll {
        tab_id: TabId(1),
        delta_css: (1.0, 0.0),
        pointer_css: Some((1.0, 1.0)),
      })
      .is_empty());

    // Pointer change is treated as "unsafe to sum": keep only the latest scroll delta in the
    // current coalescing window.
    let out = c.push(UiToWorker::Scroll {
      tab_id: TabId(1),
      delta_css: (0.0, 2.0),
      pointer_css: Some((2.0, 2.0)),
    });
    assert!(out.is_empty());

    let out2 = c.flush();
    assert_eq!(out2.len(), 1);
    match &out2[0] {
      UiToWorker::Scroll {
        tab_id,
        delta_css,
        pointer_css,
      } => {
        assert_eq!(*tab_id, TabId(1));
        assert_eq!(*delta_css, (0.0, 2.0));
        assert_eq!(*pointer_css, Some((2.0, 2.0)));
      }
      other => panic!("expected Scroll, got {other:?}"),
    }
  }

  #[test]
  fn sums_scroll_deltas_when_pointer_rounds_to_same_px() {
    let mut c = UiToWorkerRouterCoalescer::new();
    assert!(c
      .push(UiToWorker::Scroll {
        tab_id: TabId(1),
        delta_css: (1.0, 0.0),
        pointer_css: Some((10.1, 10.4)),
      })
      .is_empty());
    assert!(c
      .push(UiToWorker::Scroll {
        tab_id: TabId(1),
        delta_css: (0.0, 2.0),
        pointer_css: Some((10.2, 10.3)),
      })
      .is_empty());

    let out = c.flush();
    assert_eq!(out.len(), 1);
    match &out[0] {
      UiToWorker::Scroll {
        tab_id,
        delta_css,
        pointer_css,
      } => {
        assert_eq!(*tab_id, TabId(1));
        assert_eq!(*delta_css, (1.0, 2.0));
        // Preserve the latest pointer position for the flush.
        assert_eq!(*pointer_css, Some((10.2, 10.3)));
      }
      other => panic!("expected Scroll, got {other:?}"),
    }
  }

  #[test]
  fn coalesces_tick() {
    let mut c = UiToWorkerRouterCoalescer::new();
    assert!(c
      .push(UiToWorker::Tick {
        tab_id: TabId(1),
        delta: Duration::from_millis(5),
      })
      .is_empty());
    assert!(c
      .push(UiToWorker::Tick {
        tab_id: TabId(1),
        delta: Duration::from_millis(7),
      })
      .is_empty());
    let out = c.flush();
    assert_eq!(out.len(), 1);
    assert!(
      matches!(out[0], UiToWorker::Tick { tab_id: TabId(1), delta } if delta == Duration::from_millis(12))
    );
  }

  #[test]
  fn concatenates_text_input() {
    let mut c = UiToWorkerRouterCoalescer::new();
    assert!(c
      .push(UiToWorker::TextInput {
        tab_id: TabId(1),
        text: "a".to_string(),
      })
      .is_empty());
    assert!(c
      .push(UiToWorker::TextInput {
        tab_id: TabId(1),
        text: "bc".to_string(),
      })
      .is_empty());
    let out = c.flush();
    assert_eq!(out.len(), 1);
    match &out[0] {
      UiToWorker::TextInput { tab_id, text } => {
        assert_eq!(*tab_id, TabId(1));
        assert_eq!(text, "abc");
      }
      other => panic!("expected TextInput, got {other:?}"),
    }
  }

  #[test]
  fn keeps_latest_ime_preedit() {
    let mut c = UiToWorkerRouterCoalescer::new();
    assert!(c
      .push(UiToWorker::ImePreedit {
        tab_id: TabId(1),
        text: "a".to_string(),
        cursor: None,
      })
      .is_empty());
    assert!(c
      .push(UiToWorker::ImePreedit {
        tab_id: TabId(1),
        text: "b".to_string(),
        cursor: Some((1, 1)),
      })
      .is_empty());
    let out = c.flush();
    assert_eq!(out.len(), 1);
    match &out[0] {
      UiToWorker::ImePreedit {
        tab_id,
        text,
        cursor,
      } => {
        assert_eq!(*tab_id, TabId(1));
        assert_eq!(text, "b");
        assert_eq!(*cursor, Some((1, 1)));
      }
      other => panic!("expected ImePreedit, got {other:?}"),
    }
  }

  #[test]
  fn keeps_latest_find_query() {
    let mut c = UiToWorkerRouterCoalescer::new();
    assert!(c
      .push(UiToWorker::FindQuery {
        tab_id: TabId(1),
        query: "a".to_string(),
        case_sensitive: false,
      })
      .is_empty());
    assert!(c
      .push(UiToWorker::FindQuery {
        tab_id: TabId(1),
        query: "b".to_string(),
        case_sensitive: true,
      })
      .is_empty());
    let out = c.flush();
    assert_eq!(out.len(), 1);
    match &out[0] {
      UiToWorker::FindQuery {
        tab_id,
        query,
        case_sensitive,
      } => {
        assert_eq!(*tab_id, TabId(1));
        assert_eq!(query, "b");
        assert!(*case_sensitive);
      }
      other => panic!("expected FindQuery, got {other:?}"),
    }
  }

  #[test]
  fn keeps_latest_request_repaint() {
    let mut c = UiToWorkerRouterCoalescer::new();
    assert!(c
      .push(UiToWorker::RequestRepaint {
        tab_id: TabId(1),
        reason: RepaintReason::Scroll,
      })
      .is_empty());
    assert!(c
      .push(UiToWorker::RequestRepaint {
        tab_id: TabId(1),
        reason: RepaintReason::ViewportChanged,
      })
      .is_empty());
    let out = c.flush();
    assert_eq!(out.len(), 1);
    match &out[0] {
      UiToWorker::RequestRepaint { tab_id, reason } => {
        assert_eq!(*tab_id, TabId(1));
        assert!(matches!(reason, RepaintReason::ViewportChanged));
      }
      other => panic!("expected RequestRepaint, got {other:?}"),
    }
  }

  #[test]
  fn barrier_flushes_in_deterministic_order() {
    let mut c = UiToWorkerRouterCoalescer::new();

    assert!(c.push(pm(TabId(1), 1.0, 1.0)).is_empty());
    assert!(c
      .push(UiToWorker::ViewportChanged {
        tab_id: TabId(1),
        viewport_css: (10, 10),
        dpr: 1.0,
      })
      .is_empty());

    let out = c.push(UiToWorker::PointerDown {
      tab_id: TabId(1),
      pos_css: (5.0, 6.0),
      button: PointerButton::Primary,
      modifiers: PointerModifiers::NONE,
      click_count: 1,
    });

    assert_eq!(out.len(), 3);
    assert!(matches!(out[0], UiToWorker::PointerMove { .. }));
    assert!(matches!(out[1], UiToWorker::ViewportChanged { .. }));
    assert!(matches!(out[2], UiToWorker::PointerDown { .. }));
  }

  #[test]
  fn barrier_flush_order_is_independent_of_hashmap_iteration() {
    let mut c = UiToWorkerRouterCoalescer::new();

    assert!(c.push(pm(TabId(2), 1.0, 1.0)).is_empty());
    assert!(c.push(pm(TabId(1), 2.0, 2.0)).is_empty());

    let out = c.push(UiToWorker::Copy { tab_id: TabId(1) });

    assert_eq!(out.len(), 3);
    match &out[0] {
      UiToWorker::PointerMove { tab_id, pos_css, .. } => {
        assert_eq!(*tab_id, TabId(2));
        assert_eq!(*pos_css, (1.0, 1.0));
      }
      other => panic!("expected PointerMove for tab 2, got {other:?}"),
    }
    match &out[1] {
      UiToWorker::PointerMove { tab_id, pos_css, .. } => {
        assert_eq!(*tab_id, TabId(1));
        assert_eq!(*pos_css, (2.0, 2.0));
      }
      other => panic!("expected PointerMove for tab 1, got {other:?}"),
    }
    assert!(matches!(out[2], UiToWorker::Copy { tab_id: TabId(1) }));
  }

  #[test]
  fn ime_commit_flushes_pending_text_and_preedit_before_commit() {
    let mut c = UiToWorkerRouterCoalescer::new();
    assert!(c
      .push(UiToWorker::TextInput {
        tab_id: TabId(1),
        text: "a".to_string(),
      })
      .is_empty());
    assert!(c
      .push(UiToWorker::ImePreedit {
        tab_id: TabId(1),
        text: "b".to_string(),
        cursor: None,
      })
      .is_empty());

    let out = c.push(UiToWorker::ImeCommit {
      tab_id: TabId(1),
      text: "c".to_string(),
    });

    assert_eq!(out.len(), 3);
    assert!(matches!(out[0], UiToWorker::TextInput { .. }));
    assert!(matches!(out[1], UiToWorker::ImePreedit { .. }));
    assert!(matches!(out[2], UiToWorker::ImeCommit { .. }));
  }

  #[test]
  fn cancel_download_is_a_barrier_and_flushes_pending() {
    let mut c = UiToWorkerRouterCoalescer::new();
    assert!(c.push(pm(TabId(1), 1.0, 1.0)).is_empty());
    let out = c.push(UiToWorker::CancelDownload {
      tab_id: TabId(1),
      download_id: DownloadId(1),
    });
    assert_eq!(out.len(), 2);
    assert!(matches!(out[0], UiToWorker::PointerMove { .. }));
    assert!(matches!(out[1], UiToWorker::CancelDownload { .. }));
  }

  #[test]
  fn explicit_flush_drains_pending_state() {
    let mut c = UiToWorkerRouterCoalescer::new();
    assert!(c.push(pm(TabId(1), 1.0, 1.0)).is_empty());
    assert!(c.has_pending());
    let out = c.flush();
    assert_eq!(out.len(), 1);
    assert!(!c.has_pending());
  }
}
