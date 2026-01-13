use std::sync::Arc;

pub(crate) fn format_tab_accessible_label(
  title: &str,
  is_active: bool,
  is_pinned: bool,
  loading: bool,
  has_error: bool,
  has_warning: bool,
) -> String {
  let any_flags = is_active || is_pinned || loading || has_error || has_warning;
  if !any_flags {
    return title.to_string();
  }

  let mut part_count = 0usize;
  let mut cap = title.len() + 3; // " (" + ")"
  if is_active {
    part_count += 1;
    cap += "current tab".len();
  }
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
  if is_active {
    push_part("current tab");
  }
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
    if is_active {
      flags |= 1 << 0;
    }
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

#[cfg(test)]
mod tests {
  use super::*;
  use std::sync::Arc;

  #[test]
  fn format_tab_accessible_label_formats_active_pinned_loading_error_warning_states() {
    let title = "Example title";
    let cases = [
      (false, false, false, false, false, "Example title"),
      (
        true,
        false,
        false,
        false,
        false,
        "Example title (current tab)",
      ),
      (false, true, false, false, false, "Example title (pinned)"),
      (
        true,
        true,
        false,
        false,
        false,
        "Example title (current tab, pinned)",
      ),
      (
        true,
        true,
        true,
        false,
        false,
        "Example title (current tab, pinned, loading)",
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
        "Example title (current tab, error, warning)",
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
        "Example title (current tab, pinned, loading, error, warning)",
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
      !Arc::ptr_eq(&b, &active),
      "expected cache miss when active flag changes"
    );
    assert_eq!(active.as_ref(), "Example (current tab)");

    let active2 = cache.get_or_update("Example", true, false, false, false, false);
    assert!(Arc::ptr_eq(&active, &active2), "expected cache hit after recompute");

    let pinned = cache.get_or_update("Example", true, true, false, false, false);
    assert!(
      !Arc::ptr_eq(&active2, &pinned),
      "expected cache miss when pinned flag changes"
    );
    assert_eq!(pinned.as_ref(), "Example (current tab, pinned)");

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
    assert_eq!(loading.as_ref(), "Example (current tab, pinned, loading)");

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
    assert_eq!(error.as_ref(), "Example (current tab, pinned, loading, error)");

    let error2 = cache.get_or_update("Example", true, true, true, true, false);
    assert!(Arc::ptr_eq(&error, &error2), "expected cache hit after recompute");

    let warning = cache.get_or_update("Example", true, true, true, true, true);
    assert!(
      !Arc::ptr_eq(&error2, &warning),
      "expected cache miss when warning flag changes"
    );
    assert_eq!(
      warning.as_ref(),
      "Example (current tab, pinned, loading, error, warning)"
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
      "Example 2 (current tab, pinned, loading, error, warning)"
    );

    let f = cache.get_or_update("Example 2", true, true, true, true, true);
    assert!(Arc::ptr_eq(&e, &f), "expected cache hit after recompute");
  }
}
