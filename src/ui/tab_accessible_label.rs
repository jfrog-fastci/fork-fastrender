use std::sync::Arc;

pub(crate) fn format_tab_accessible_label(
  title: &str,
  is_active: bool,
  is_pinned: bool,
  loading: bool,
  has_error: bool,
  has_warning: bool,
) -> String {
  // Selection state is conveyed via AccessKit `selected=true/false` on the tab node.
  // Do not encode "current tab" (or similar) into the accessible *name*.
  let _ = is_active;
  let any_flags = is_pinned || loading || has_error || has_warning;
  if !any_flags {
    return title.to_string();
  }

  let mut part_count = 0usize;
  let mut cap = title.len() + 3; // " (" + ")"
  if is_pinned {
    part_count += 1;
    cap += "pinned".len();
  }
  if loading {
    part_count += 1;
    cap += "loading".len();
  }
  if has_error {
    part_count += 1;
    cap += "error".len();
  }
  if has_warning {
    part_count += 1;
    cap += "warning".len();
  }
  // ", " separators between parts.
  if part_count > 1 {
    cap += (part_count - 1) * 2;
  }

  let mut label = String::with_capacity(cap);
  label.push_str(title);
  label.push_str(" (");
  let mut first = true;
  let mut push_part = |part: &'static str| {
    if !first {
      label.push_str(", ");
    } else {
      first = false;
    }
    label.push_str(part);
  };
  if is_pinned {
    push_part("pinned");
  }
  if loading {
    push_part("loading");
  }
  if has_error {
    push_part("error");
  }
  if has_warning {
    push_part("warning");
  }
  label.push(')');
  label
}

#[derive(Debug, Clone, Default)]
pub struct TabAccessibleLabelCache {
  entry: Option<TabAccessibleLabelCacheEntry>,
}

#[derive(Debug, Clone)]
struct TabAccessibleLabelCacheEntry {
  title: Arc<str>,
  flags: u8,
  label: Arc<str>,
}

impl TabAccessibleLabelCache {
  pub fn get_or_update(
    &mut self,
    title: &str,
    is_active: bool,
    is_pinned: bool,
    loading: bool,
    has_error: bool,
    has_warning: bool,
  ) -> Arc<str> {
    let mut flags: u8 = 0;
    // Selection state is conveyed separately via AccessKit `selected=true/false` on the tab node.
    // Do not key the name cache on selection.
    let _ = is_active;
    if is_pinned {
      flags |= 1 << 1;
    }
    if loading {
      flags |= 1 << 2;
    }
    if has_error {
      flags |= 1 << 3;
    }
    if has_warning {
      flags |= 1 << 4;
    }

    if let Some(entry) = &self.entry {
      if entry.flags == flags && entry.title.as_ref() == title {
        return Arc::clone(&entry.label);
      }
    }

    // Update cache.
    let title_arc: Arc<str> = Arc::from(title);
    let label_arc: Arc<str> = if flags == 0 {
      Arc::clone(&title_arc)
    } else {
      Arc::from(format_tab_accessible_label(
        title,
        is_active,
        is_pinned,
        loading,
        has_error,
        has_warning,
      ))
    };
    self.entry = Some(TabAccessibleLabelCacheEntry {
      title: title_arc,
      flags,
      label: Arc::clone(&label_arc),
    });
    label_arc
  }
}

pub(crate) fn format_title_prefixed_accessible_label(prefix: &str, title: &str) -> String {
  // Common case: both strings are non-empty.
  let mut out = String::with_capacity(prefix.len() + 2 + title.len());
  out.push_str(prefix);
  out.push_str(": ");
  out.push_str(title);
  out
}

fn format_tab_search_row_accessible_label(title: &str, secondary: &str) -> String {
  if secondary.trim().is_empty() {
    return format_title_prefixed_accessible_label("Switch to tab", title);
  }

  let prefix = "Switch to tab";
  let mut out = String::with_capacity(prefix.len() + 2 + title.len() + 2 + secondary.len() + 1);
  out.push_str(prefix);
  out.push_str(": ");
  out.push_str(title);
  out.push_str(" (");
  out.push_str(secondary);
  out.push(')');
  out
}

#[derive(Debug, Clone, Default)]
pub struct TitlePrefixedLabelCache {
  entry: Option<TitlePrefixedLabelCacheEntry>,
}

#[derive(Debug, Clone)]
struct TitlePrefixedLabelCacheEntry {
  prefix: &'static str,
  title: Arc<str>,
  label: Arc<str>,
}

impl TitlePrefixedLabelCache {
  pub fn get_or_update(&mut self, prefix: &'static str, title: &str) -> Arc<str> {
    if let Some(entry) = &self.entry {
      if entry.prefix == prefix && entry.title.as_ref() == title {
        return Arc::clone(&entry.label);
      }
    }

    let title_arc: Arc<str> = Arc::from(title);
    let label_arc: Arc<str> = Arc::from(format_title_prefixed_accessible_label(prefix, title));
    self.entry = Some(TitlePrefixedLabelCacheEntry {
      prefix,
      title: title_arc,
      label: Arc::clone(&label_arc),
    });
    label_arc
  }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct TabSearchRowAccessibleLabelCache {
  entry: Option<TabSearchRowAccessibleLabelCacheEntry>,
}

#[derive(Debug, Clone)]
struct TabSearchRowAccessibleLabelCacheEntry {
  title: Arc<str>,
  secondary: Arc<str>,
  label: Arc<str>,
}

impl TabSearchRowAccessibleLabelCache {
  pub fn get_or_update(&mut self, title: &str, secondary: &str) -> Arc<str> {
    let secondary_for_cache = if secondary.trim().is_empty() { "" } else { secondary };

    if let Some(entry) = &self.entry {
      if entry.title.as_ref() == title && entry.secondary.as_ref() == secondary_for_cache {
        return Arc::clone(&entry.label);
      }
    }

    let title_arc: Arc<str> = Arc::from(title);
    let secondary_arc: Arc<str> = Arc::from(secondary_for_cache);
    let label_arc: Arc<str> = Arc::from(format_tab_search_row_accessible_label(
      title,
      secondary_for_cache,
    ));

    self.entry = Some(TabSearchRowAccessibleLabelCacheEntry {
      title: title_arc,
      secondary: secondary_arc,
      label: Arc::clone(&label_arc),
    });

    label_arc
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::Arc;

  #[test]
  fn format_tab_accessible_label_formats_active_pinned_loading_error_warning_states() {
    let title = "Example title";
    let cases = [
      (false, false, false, false, false, "Example title"),
      (true, false, false, false, false, "Example title"),
      (false, true, false, false, false, "Example title (pinned)"),
      (true, true, false, false, false, "Example title (pinned)"),
      (
        true,
        true,
        true,
        false,
        false,
        "Example title (pinned, loading)",
      ),
      (
        false,
        true,
        false,
        true,
        false,
        "Example title (pinned, error)",
      ),
      (
        true,
        false,
        false,
        true,
        true,
        "Example title (error, warning)",
      ),
      (
        false,
        true,
        true,
        true,
        true,
        "Example title (pinned, loading, error, warning)",
      ),
      (
        true,
        true,
        true,
        true,
        true,
        "Example title (pinned, loading, error, warning)",
      ),
    ];
    for (is_active, is_pinned, loading, err, warn, expected) in cases {
      assert_eq!(
        format_tab_accessible_label(title, is_active, is_pinned, loading, err, warn),
        expected
      );
    }
  }

  #[test]
  fn tab_accessible_label_cache_updates_and_is_stable_when_inputs_unchanged() {
    let mut cache = TabAccessibleLabelCache::default();

    let a = cache.get_or_update("Example", false, false, false, false, false);
    let b = cache.get_or_update("Example", false, false, false, false, false);
    assert!(
      Arc::ptr_eq(&a, &b),
      "expected cache hit to reuse allocation"
    );
    assert_eq!(a.as_ref(), "Example");

    let active = cache.get_or_update("Example", true, false, false, false, false);
    assert!(
      Arc::ptr_eq(&b, &active),
      "expected cache hit when active flag changes (selection does not affect the name)"
    );
    assert_eq!(active.as_ref(), "Example");

    let active2 = cache.get_or_update("Example", true, false, false, false, false);
    assert!(Arc::ptr_eq(&active, &active2), "expected cache hit after recompute");

    let pinned = cache.get_or_update("Example", true, true, false, false, false);
    assert!(
      !Arc::ptr_eq(&active2, &pinned),
      "expected cache miss when pinned flag changes"
    );
    assert_eq!(pinned.as_ref(), "Example (pinned)");

    let pinned2 = cache.get_or_update("Example", true, true, false, false, false);
    assert!(
      Arc::ptr_eq(&pinned, &pinned2),
      "expected cache hit after recompute"
    );

    let loading = cache.get_or_update("Example", true, true, true, false, false);
    assert!(
      !Arc::ptr_eq(&pinned2, &loading),
      "expected cache miss when loading flag changes"
    );
    assert_eq!(loading.as_ref(), "Example (pinned, loading)");

    let loading2 = cache.get_or_update("Example", true, true, true, false, false);
    assert!(
      Arc::ptr_eq(&loading, &loading2),
      "expected cache hit after recompute"
    );

    let error = cache.get_or_update("Example", true, true, true, true, false);
    assert!(
      !Arc::ptr_eq(&loading2, &error),
      "expected cache miss when error flag changes"
    );
    assert_eq!(error.as_ref(), "Example (pinned, loading, error)");

    let error2 = cache.get_or_update("Example", true, true, true, true, false);
    assert!(Arc::ptr_eq(&error, &error2), "expected cache hit after recompute");

    let warning = cache.get_or_update("Example", true, true, true, true, true);
    assert!(
      !Arc::ptr_eq(&error2, &warning),
      "expected cache miss when warning flag changes"
    );
    assert_eq!(
      warning.as_ref(),
      "Example (pinned, loading, error, warning)"
    );

    let warning2 = cache.get_or_update("Example", true, true, true, true, true);
    assert!(
      Arc::ptr_eq(&warning, &warning2),
      "expected cache hit after recompute"
    );

    let e = cache.get_or_update("Example 2", true, true, true, true, true);
    assert!(
      !Arc::ptr_eq(&warning2, &e),
      "expected cache miss when title changes"
    );
    assert_eq!(
      e.as_ref(),
      "Example 2 (pinned, loading, error, warning)"
    );

    let f = cache.get_or_update("Example 2", true, true, true, true, true);
    assert!(Arc::ptr_eq(&e, &f), "expected cache hit after recompute");
  }

  #[test]
  fn title_prefixed_label_cache_reuses_allocation_until_inputs_change() {
    let mut cache = TitlePrefixedLabelCache::default();
    let a = cache.get_or_update("Close tab", "Example");
    let b = cache.get_or_update("Close tab", "Example");
    assert!(Arc::ptr_eq(&a, &b), "expected cache hit");
    assert_eq!(a.as_ref(), "Close tab: Example");

    let c = cache.get_or_update("Close tab", "Other");
    assert!(!Arc::ptr_eq(&b, &c), "expected cache miss when title changes");
    assert_eq!(c.as_ref(), "Close tab: Other");

    let d = cache.get_or_update("New tab", "Other");
    assert!(
      !Arc::ptr_eq(&c, &d),
      "expected cache miss when prefix changes"
    );
    assert_eq!(d.as_ref(), "New tab: Other");
  }

  #[test]
  fn tab_search_row_accessible_label_cache_formats_secondary_optional() {
    let mut cache = TabSearchRowAccessibleLabelCache::default();

    let no_secondary = cache.get_or_update("Example", "");
    assert_eq!(no_secondary.as_ref(), "Switch to tab: Example");

    let with_secondary = cache.get_or_update("Example", "example.com");
    assert_eq!(
      with_secondary.as_ref(),
      "Switch to tab: Example (example.com)"
    );
  }

  #[test]
  fn tab_search_row_accessible_label_cache_reuses_allocation_until_inputs_change() {
    let mut cache = TabSearchRowAccessibleLabelCache::default();
    let a = cache.get_or_update("Example", "example.com");
    let b = cache.get_or_update("Example", "example.com");
    assert!(Arc::ptr_eq(&a, &b), "expected cache hit");

    let c = cache.get_or_update("Example", "");
    assert!(!Arc::ptr_eq(&b, &c), "expected cache miss when secondary changes");
    assert_eq!(c.as_ref(), "Switch to tab: Example");

    let d = cache.get_or_update("Other", "");
    assert!(!Arc::ptr_eq(&c, &d), "expected cache miss when title changes");
    assert_eq!(d.as_ref(), "Switch to tab: Other");
  }
}
