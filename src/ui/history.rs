use std::collections::HashMap;
use std::time::SystemTime;

use crate::geometry::Point;
use crate::scroll::ScrollState;

#[derive(Debug, Clone)]
pub struct HistoryEntry {
  pub url: String,
  pub scroll_x: f32,
  pub scroll_y: f32,
  /// Scroll offsets for element scroll containers keyed by box_id.
  ///
  /// This is sourced from [`ScrollState::elements`] so history back/forward can restore nested
  /// scrollers in addition to the viewport scroll.
  pub element_scroll: HashMap<usize, Point>,
  pub title: Option<String>,
  pub timestamp: Option<SystemTime>,
}

impl HistoryEntry {
  fn new(url: String) -> Self {
    Self {
      url,
      scroll_x: 0.0,
      scroll_y: 0.0,
      element_scroll: HashMap::new(),
      title: None,
      timestamp: None,
    }
  }

  pub fn scroll_state(&self) -> ScrollState {
    ScrollState::from_parts(
      Point::new(self.scroll_x, self.scroll_y),
      self.element_scroll.clone(),
    )
  }
}

#[derive(Debug, Clone, Default)]
pub struct TabHistory {
  entries: Vec<HistoryEntry>,
  index: Option<usize>,
  /// Index of the last committed history entry.
  ///
  /// The worker can advance `index` optimistically while a navigation is in-flight (for example
  /// `GoBack`/`GoForward`, or provisional entries pushed for typed navigations). `committed_index`
  /// tracks the entry that still corresponds to the currently committed document so that
  /// cancellation (StopLoading) can restore back/forward invariants and avoid leaving cancelled
  /// navigations as the active history entry.
  committed_index: Option<usize>,
}

impl TabHistory {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn with_initial(url: String) -> Self {
    Self {
      entries: vec![HistoryEntry::new(url)],
      index: Some(0),
      committed_index: Some(0),
    }
  }

  pub fn current(&self) -> Option<&HistoryEntry> {
    self.index.and_then(|i| self.entries.get(i))
  }

  pub fn can_go_back(&self) -> bool {
    self.index.is_some_and(|i| i > 0)
  }

  pub fn can_go_forward(&self) -> bool {
    self.index.is_some_and(|i| i + 1 < self.entries.len())
  }

  pub fn len(&self) -> usize {
    self.entries.len()
  }

  /// Pushes a new entry and truncates any forward history.
  ///
  /// Consecutive duplicate URLs are deduped (pushing the current URL is a no-op).
  pub fn push(&mut self, url: String) {
    if self.current().is_some_and(|entry| entry.url == url) {
      return;
    }

    match self.index {
      None => {
        self.entries.push(HistoryEntry::new(url));
        self.index = Some(0);
      }
      Some(i) => {
        self.entries.truncate(i + 1);
        self.entries.push(HistoryEntry::new(url));
        self.index = Some(i + 1);
      }
    }
  }

  /// Replace the current history entry's URL.
  ///
  /// This is used when a navigation is superseded before it commits: the worker may have pushed a
  /// provisional history entry for the in-flight navigation, and should update it in-place to avoid
  /// leaving cancelled URLs in the back/forward list.
  pub fn replace_current_url(&mut self, url: String) {
    match self.index {
      None => self.push(url),
      Some(i) => {
        let Some(entry) = self.entries.get_mut(i) else {
          debug_assert!(false, "TabHistory invariant violated: index out of bounds");
          return;
        };
        entry.url = url;
      }
    }
  }

  /// Mark the current history entry as committed.
  pub fn mark_committed(&mut self) {
    self.committed_index = self.index;
  }

  /// Restore `index` back to the last committed entry.
  pub fn revert_to_committed(&mut self) {
    let Some(committed) = self.committed_index else {
      return;
    };
    if committed < self.entries.len() {
      self.index = Some(committed);
    } else if self.entries.is_empty() {
      self.index = None;
      self.committed_index = None;
    } else {
      // Defensive: clamp to the last entry and consider it committed so invariants stay sane.
      let last = self.entries.len().saturating_sub(1);
      self.index = Some(last);
      self.committed_index = Some(last);
    }
  }

  /// Cancel a provisional history entry for an in-flight navigation.
  ///
  /// This is intended for typed/link-click navigations that optimistically pushed a provisional
  /// history entry before commit. On cancellation, we remove that entry entirely so it does not
  /// appear as the current entry (or as a forward entry).
  pub fn cancel_pending_navigation_entry(&mut self) {
    let Some(i) = self.index else {
      return;
    };
    if i >= self.entries.len() {
      debug_assert!(false, "TabHistory invariant violated: index out of bounds");
      self.revert_to_committed();
      return;
    }

    self.entries.remove(i);

    // Update the committed index when removing entries before it (defensive; stop-loading should
    // only remove provisional entries that were appended after the committed entry).
    if let Some(committed) = self.committed_index {
      if committed == i {
        self.committed_index = None;
      } else if committed > i {
        self.committed_index = Some(committed - 1);
      }
    }

    if self.entries.is_empty() {
      self.index = None;
      self.committed_index = None;
      return;
    }

    // Prefer restoring to the last committed entry when available.
    if let Some(committed) = self.committed_index {
      if committed < self.entries.len() {
        self.index = Some(committed);
        return;
      }
    }

    // Otherwise, fall back to the nearest remaining entry.
    if i > 0 {
      self.index = Some(i - 1);
    } else {
      self.index = Some(0);
    }
  }

  pub fn update_scroll(&mut self, scroll_x: f32, scroll_y: f32) {
    let Some(i) = self.index else {
      return;
    };
    let Some(entry) = self.entries.get_mut(i) else {
      debug_assert!(false, "TabHistory invariant violated: index out of bounds");
      return;
    };

    entry.scroll_x = scroll_x;
    entry.scroll_y = scroll_y;
  }

  pub fn update_scroll_state(&mut self, scroll_state: &ScrollState) {
    let Some(i) = self.index else {
      return;
    };
    let Some(entry) = self.entries.get_mut(i) else {
      debug_assert!(false, "TabHistory invariant violated: index out of bounds");
      return;
    };

    entry.scroll_x = scroll_state.viewport.x;
    entry.scroll_y = scroll_state.viewport.y;
    entry.element_scroll = scroll_state.elements.clone();
  }

  pub fn set_title(&mut self, title: String) {
    let Some(i) = self.index else {
      return;
    };
    let Some(entry) = self.entries.get_mut(i) else {
      debug_assert!(false, "TabHistory invariant violated: index out of bounds");
      return;
    };

    entry.title = Some(title);
  }

  pub fn go_back(&mut self) -> Option<&HistoryEntry> {
    let i = self.index?;
    if i == 0 {
      return None;
    }
    self.index = Some(i - 1);
    self.current()
  }

  pub fn go_forward(&mut self) -> Option<&HistoryEntry> {
    let i = self.index?;
    if i + 1 >= self.entries.len() {
      return None;
    }
    self.index = Some(i + 1);
    self.current()
  }

  /// Attempts a back/forward navigation to `target_url`, but only when it matches the immediate
  /// back or forward history entry.
  ///
  /// This avoids surprising "global search" jumps when the UI provides a stale URL (e.g. after
  /// rapid navigations).
  pub fn go_back_forward_to(&mut self, target_url: &str) -> Option<&HistoryEntry> {
    let i = self.index?;

    if i > 0
      && self
        .entries
        .get(i - 1)
        .is_some_and(|entry| entry.url == target_url)
    {
      self.index = Some(i - 1);
      return self.current();
    }

    if i + 1 < self.entries.len()
      && self
        .entries
        .get(i + 1)
        .is_some_and(|entry| entry.url == target_url)
    {
      self.index = Some(i + 1);
      return self.current();
    }

    None
  }

  pub fn reload_target(&self) -> Option<&HistoryEntry> {
    self.current()
  }

  /// Finalizes a navigation that was previously initiated with `push`.
  ///
  /// This supports HTTP redirects: if `final_url` differs from the URL that was pushed, the
  /// current entry's URL is updated *in place* (no new history entry is created).
  ///
  /// `original_url` is used as a guard so that late completion of an older navigation doesn't
  /// overwrite a newer one.
  pub fn commit_navigation(
    &mut self,
    original_url: &str,
    final_url: Option<&str>,
  ) -> Option<&HistoryEntry> {
    let Some(i) = self.index else {
      return None;
    };
    let Some(entry) = self.entries.get_mut(i) else {
      debug_assert!(false, "TabHistory invariant violated: index out of bounds");
      return None;
    };

    // Guard against stale completions: do not mark a navigation as committed unless the current
    // entry still matches the navigation's original URL.
    if entry.url != original_url {
      return self.current();
    }

    if let Some(final_url) = final_url {
      if final_url != original_url {
        entry.url = final_url.to_string();
      }
    }

    entry.timestamp = Some(SystemTime::now());
    self.committed_index = self.index;
    self.current()
  }
}

#[cfg(test)]
mod tests {
  use super::TabHistory;

  #[test]
  fn empty_history_edge_cases() {
    let mut history = TabHistory::new();
    assert_eq!(history.len(), 0);
    assert!(history.current().is_none());
    assert!(!history.can_go_back());
    assert!(!history.can_go_forward());
    assert!(history.go_back().is_none());
    assert!(history.go_forward().is_none());
    assert!(history.reload_target().is_none());

    history.update_scroll(1.0, 2.0);
    history.set_title("ignored".to_string());
    assert!(history.commit_navigation("a", Some("b")).is_none());

    history.push("https://example.com".to_string());
    assert_eq!(history.len(), 1);
    assert_eq!(history.current().unwrap().url, "https://example.com");
  }

  #[test]
  fn push_back_forward() {
    let mut history = TabHistory::with_initial("a".to_string());
    assert_eq!(history.current().unwrap().url, "a");

    history.push("b".to_string());
    assert_eq!(history.current().unwrap().url, "b");
    assert!(history.can_go_back());
    assert!(!history.can_go_forward());

    let back = history.go_back().unwrap();
    assert_eq!(back.url, "a");
    assert!(!history.can_go_back());
    assert!(history.can_go_forward());

    let forward = history.go_forward().unwrap();
    assert_eq!(forward.url, "b");
    assert!(history.can_go_back());
    assert!(!history.can_go_forward());
  }

  #[test]
  fn forward_history_is_truncated_after_push() {
    let mut history = TabHistory::with_initial("a".to_string());
    history.push("b".to_string());
    history.push("c".to_string());
    assert_eq!(history.len(), 3);

    assert_eq!(history.go_back().unwrap().url, "b");
    assert!(history.can_go_forward());

    history.push("d".to_string());
    assert_eq!(history.len(), 3);
    assert_eq!(history.current().unwrap().url, "d");
    assert!(!history.can_go_forward());

    assert_eq!(history.go_back().unwrap().url, "b");
    assert_eq!(history.go_back().unwrap().url, "a");
    assert!(history.go_back().is_none());

    assert_eq!(history.go_forward().unwrap().url, "b");
    assert_eq!(history.go_forward().unwrap().url, "d");
    assert!(history.go_forward().is_none());
  }

  #[test]
  fn scroll_is_restored_across_navigation() {
    let mut history = TabHistory::with_initial("a".to_string());
    history.update_scroll(10.0, 20.0);

    history.push("b".to_string());
    history.update_scroll(1.0, 2.0);

    let back = history.go_back().unwrap();
    assert_eq!(back.url, "a");
    assert_eq!(back.scroll_x, 10.0);
    assert_eq!(back.scroll_y, 20.0);

    let forward = history.go_forward().unwrap();
    assert_eq!(forward.url, "b");
    assert_eq!(forward.scroll_x, 1.0);
    assert_eq!(forward.scroll_y, 2.0);
  }

  #[test]
  fn redirect_commit_updates_current_entry_in_place() {
    let mut history = TabHistory::with_initial("start".to_string());
    history.push("http://example.com".to_string());
    assert_eq!(history.len(), 2);

    history.commit_navigation("http://example.com", Some("https://example.com"));
    assert_eq!(history.len(), 2);
    assert_eq!(history.current().unwrap().url, "https://example.com");

    // The guard should prevent overwriting after the URL has already changed.
    history.commit_navigation("http://example.com", Some("https://wrong.example.com"));
    assert_eq!(history.current().unwrap().url, "https://example.com");
  }

  #[test]
  fn consecutive_duplicate_urls_are_deduped() {
    let mut history = TabHistory::with_initial("a".to_string());
    history.push("a".to_string());
    assert_eq!(history.len(), 1);
    assert_eq!(history.current().unwrap().url, "a");

    history.push("b".to_string());
    history.push("b".to_string());
    assert_eq!(history.len(), 2);
    assert_eq!(history.current().unwrap().url, "b");
  }
}
