use crate::ui::messages::TabId;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Browser-side gate for clipboard writes requested by an untrusted worker.
///
/// The worker is only allowed to request a clipboard write (`WorkerToUi::SetClipboardText`) when the
/// browser UI has explicitly initiated a clipboard operation for the same tab (currently
/// [`UiToWorker::Copy`] or [`UiToWorker::Cut`]).
///
/// This lives in `src/ui/` so it can be unit-tested without pulling in any windowing/egui types.
#[derive(Debug, Clone)]
pub struct ClipboardWriteGate {
  pending_tabs: HashMap<TabId, Instant>,
  ignored_writes_logged: usize,
  max_ignored_write_logs: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardWriteGateDecision {
  /// The clipboard write is allowed and consumes the pending request for the tab.
  Allowed,
  /// The clipboard write is rejected because there is no matching pending UI request.
  Rejected {
    /// True when the caller should surface a bounded warning/debug log entry.
    should_log: bool,
  },
}

impl Default for ClipboardWriteGate {
  fn default() -> Self {
    Self::new()
  }
}

impl ClipboardWriteGate {
  /// Maximum age of an armed clipboard write request.
  ///
  /// In a multiprocess model, the renderer is untrusted. Without a timeout a compromised worker
  /// could "bank" a Copy/Cut arm indefinitely (by never responding) and then overwrite the OS
  /// clipboard at an arbitrary later time.
  const ARM_TIMEOUT: Duration = Duration::from_secs(1);
  const DEFAULT_MAX_IGNORED_WRITE_LOGS: usize = 3;

  pub fn new() -> Self {
    Self::with_max_ignored_write_logs(Self::DEFAULT_MAX_IGNORED_WRITE_LOGS)
  }

  pub fn with_max_ignored_write_logs(max_ignored_write_logs: usize) -> Self {
    Self {
      pending_tabs: HashMap::new(),
      ignored_writes_logged: 0,
      max_ignored_write_logs,
    }
  }

  fn register_at(&mut self, tab_id: TabId, now: Instant) {
    self.pending_tabs.insert(tab_id, now);
  }

  /// Register an explicit copy request for `tab_id` originating from the browser UI.
  pub fn register_copy(&mut self, tab_id: TabId) {
    self.register_at(tab_id, Instant::now());
  }

  /// Register an explicit cut request for `tab_id` originating from the browser UI.
  pub fn register_cut(&mut self, tab_id: TabId) {
    self.register_at(tab_id, Instant::now());
  }

  /// Clear any pending clipboard write request for `tab_id` (e.g. when a tab is closed).
  pub fn clear_tab(&mut self, tab_id: TabId) {
    self.pending_tabs.remove(&tab_id);
  }

  /// Clear all pending clipboard write requests (e.g. when switching tabs).
  pub fn clear_all(&mut self) {
    self.pending_tabs.clear();
  }

  fn reject(&mut self) -> ClipboardWriteGateDecision {
    let should_log = self.ignored_writes_logged < self.max_ignored_write_logs;
    if should_log {
      self.ignored_writes_logged += 1;
    }
    ClipboardWriteGateDecision::Rejected { should_log }
  }

  /// Decide whether to honor a `WorkerToUi::SetClipboardText` for `tab_id`.
  ///
  /// When allowed, the pending request for this tab is consumed so at most one clipboard write is
  /// accepted per UI-initiated copy/cut action.
  pub fn on_worker_set_clipboard_text(&mut self, tab_id: TabId) -> ClipboardWriteGateDecision {
    self.on_worker_set_clipboard_text_at(tab_id, Instant::now())
  }

  fn on_worker_set_clipboard_text_at(
    &mut self,
    tab_id: TabId,
    now: Instant,
  ) -> ClipboardWriteGateDecision {
    let Some(armed_at) = self.pending_tabs.remove(&tab_id) else {
      return self.reject();
    };

    let elapsed = now
      .checked_duration_since(armed_at)
      .unwrap_or(Duration::MAX);
    if elapsed > Self::ARM_TIMEOUT {
      return self.reject();
    }

    ClipboardWriteGateDecision::Allowed
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn rejects_clipboard_write_without_pending_request() {
    let mut gate = ClipboardWriteGate::new();
    let tab_id = TabId(1);
    assert!(matches!(
      gate.on_worker_set_clipboard_text(tab_id),
      ClipboardWriteGateDecision::Rejected { .. }
    ));
    // Rejected write should not create a pending entry.
    assert!(matches!(
      gate.on_worker_set_clipboard_text(tab_id),
      ClipboardWriteGateDecision::Rejected { .. }
    ));
  }

  #[test]
  fn accepts_exactly_one_write_per_registered_copy_or_cut() {
    let mut gate = ClipboardWriteGate::new();
    let tab_id = TabId(42);

    gate.register_copy(tab_id);
    assert!(matches!(
      gate.on_worker_set_clipboard_text(tab_id),
      ClipboardWriteGateDecision::Allowed
    ));
    assert!(matches!(
      gate.on_worker_set_clipboard_text(tab_id),
      ClipboardWriteGateDecision::Rejected { .. }
    ));

    gate.register_cut(tab_id);
    assert!(matches!(
      gate.on_worker_set_clipboard_text(tab_id),
      ClipboardWriteGateDecision::Allowed
    ));
    assert!(matches!(
      gate.on_worker_set_clipboard_text(tab_id),
      ClipboardWriteGateDecision::Rejected { .. }
    ));
  }

  #[test]
  fn clearing_on_tab_close_removes_pending_request() {
    let mut gate = ClipboardWriteGate::new();
    let tab_id = TabId(7);

    gate.register_copy(tab_id);
    gate.clear_tab(tab_id);
    assert!(matches!(
      gate.on_worker_set_clipboard_text(tab_id),
      ClipboardWriteGateDecision::Rejected { .. }
    ));
  }

  #[test]
  fn rejects_clipboard_write_after_timeout() {
    let mut gate = ClipboardWriteGate::new();
    let tab_id = TabId(5);
    let armed_at = Instant::now();
    gate.register_at(tab_id, armed_at);

    let late = armed_at + ClipboardWriteGate::ARM_TIMEOUT + Duration::from_millis(1);
    assert!(matches!(
      gate.on_worker_set_clipboard_text_at(tab_id, late),
      ClipboardWriteGateDecision::Rejected { .. }
    ));
  }

  #[test]
  fn rejection_logging_is_bounded() {
    let mut gate = ClipboardWriteGate::with_max_ignored_write_logs(1);
    let tab_id = TabId(99);

    assert_eq!(
      gate.on_worker_set_clipboard_text(tab_id),
      ClipboardWriteGateDecision::Rejected { should_log: true }
    );
    assert_eq!(
      gate.on_worker_set_clipboard_text(tab_id),
      ClipboardWriteGateDecision::Rejected { should_log: false }
    );
  }
}
