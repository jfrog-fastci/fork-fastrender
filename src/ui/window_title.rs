use crate::ui::TabId;

const APP_NAME: &str = "FastRender";
const APP_SUFFIX: &str = " — FastRender";

/// Cache/state for synchronizing the OS window title from browser tab state without per-frame
/// allocations.
///
/// The steady-state `sync` path is allocation-free:
/// - Compare the desired tab title source (`&str`) against the cached owned source.
/// - Only rebuild the full window title string when the displayed tab title actually changes.
#[derive(Debug, Default)]
pub struct WindowTitleCache {
  active_tab_id: Option<TabId>,
  /// Cached displayed tab title source (tab title, URL, or fallback).
  ///
  /// Stored as an owned `String` so we can compare against the next frame's borrowed `&str` without
  /// allocating.
  source: Option<String>,
  /// Cached full window title string last set on the OS window.
  full_title: String,
}

impl WindowTitleCache {
  /// Cached active tab id from the last `sync` call.
  pub fn active_tab_id(&self) -> Option<TabId> {
    self.active_tab_id
  }

  /// Cached full window title (including `" — FastRender"` suffix for tabbed titles).
  pub fn full_title(&self) -> &str {
    self.full_title.as_str()
  }

  /// Update caches and, if needed, rebuild the full window title.
  ///
  /// Returns `Some(&str)` containing the new full title when the caller should call
  /// `window.set_title(...)`.
  ///
  /// `display_title_source` should be the *already-selected* title source for the active tab:
  /// - `Some(&tab.title)` when the tab has a non-empty title,
  /// - otherwise `Some(&tab.current_url)`,
  /// - otherwise `Some("New Tab")`,
  /// - `None` when there is no active tab (window title becomes `"FastRender"`).
  pub fn sync(
    &mut self,
    active_tab_id: Option<TabId>,
    display_title_source: Option<&str>,
  ) -> Option<&str> {
    // Cache the active tab id so callers can detect which tab the window title reflects.
    self.active_tab_id = active_tab_id;

    // Fast path: nothing changed.
    if display_title_source == self.source.as_deref() {
      // `WindowTitleCache::default()` starts with an empty `full_title`. Ensure we still produce the
      // correct initial title the first time `sync` is called.
      let full_title_matches = match display_title_source {
        Some(src) => {
          self.full_title.len() == src.len() + APP_SUFFIX.len()
            && self.full_title.starts_with(src)
            && self.full_title.ends_with(APP_SUFFIX)
        }
        None => self.full_title == APP_NAME,
      };
      if full_title_matches {
        return None;
      }
      // `source` matched but `full_title` did not; fall through to rebuild.
    }

    // The title source changed; update the owned cache (reusing capacity when possible).
    match display_title_source {
      Some(src) => {
        if let Some(existing) = self.source.as_mut() {
          existing.clear();
          existing.push_str(src);
        } else {
          self.source = Some(src.to_string());
        }

        self.full_title.clear();
        self.full_title.push_str(src);
        self.full_title.push_str(APP_SUFFIX);
      }
      None => {
        self.source = None;
        self.full_title.clear();
        self.full_title.push_str(APP_NAME);
      }
    }

    Some(self.full_title.as_str())
  }
}

#[cfg(test)]
mod tests {
  use super::WindowTitleCache;
  use crate::ui::TabId;

  #[test]
  fn window_title_is_app_name_when_no_tab() {
    let mut cache = WindowTitleCache::default();

    assert_eq!(cache.sync(None, None), Some("FastRender"));
    assert_eq!(cache.full_title(), "FastRender");
    assert_eq!(cache.active_tab_id(), None);

    // Steady-state path should be allocation-free and avoid redundant updates.
    assert_eq!(cache.sync(None, None), None);
    assert_eq!(cache.full_title(), "FastRender");
  }

  #[test]
  fn window_title_updates_when_active_tab_changes() {
    let mut cache = WindowTitleCache::default();
    let tab_a = TabId::new();
    let tab_b = TabId::new();

    assert_eq!(
      cache.sync(Some(tab_a), Some("Example A")),
      Some("Example A — FastRender")
    );
    assert_eq!(
      cache.sync(Some(tab_b), Some("Example B")),
      Some("Example B — FastRender")
    );
  }

  #[test]
  fn window_title_cache_tracks_active_tab_even_when_title_unchanged() {
    let mut cache = WindowTitleCache::default();
    let tab_a = TabId::new();
    let tab_b = TabId::new();

    assert_eq!(cache.sync(Some(tab_a), Some("Same")), Some("Same — FastRender"));
    assert_eq!(cache.active_tab_id(), Some(tab_a));
    assert_eq!(cache.full_title(), "Same — FastRender");

    // Title unchanged (so callers can skip `window.set_title`), but we still want the cache to
    // reflect which tab is active for any consumers of `active_tab_id()`.
    assert_eq!(cache.sync(Some(tab_b), Some("Same")), None);
    assert_eq!(cache.active_tab_id(), Some(tab_b));
    assert_eq!(cache.full_title(), "Same — FastRender");
  }

  #[test]
  fn window_title_updates_when_tab_title_changes() {
    let mut cache = WindowTitleCache::default();
    let tab = TabId::new();

    assert_eq!(
      cache.sync(Some(tab), Some("Before")),
      Some("Before — FastRender")
    );
    assert_eq!(cache.sync(Some(tab), Some("After")), Some("After — FastRender"));
  }

  #[test]
  fn window_title_does_not_update_when_unchanged() {
    let mut cache = WindowTitleCache::default();
    let tab = TabId::new();

    assert_eq!(
      cache.sync(Some(tab), Some("Stable")),
      Some("Stable — FastRender")
    );
    assert_eq!(cache.sync(Some(tab), Some("Stable")), None);
  }
}
