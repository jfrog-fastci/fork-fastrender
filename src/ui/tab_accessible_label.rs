pub(crate) fn format_tab_accessible_label(
  title: &str,
  is_active: bool,
  is_pinned: bool,
  loading: bool,
  has_error: bool,
  has_warning: bool,
) -> String {
  let mut parts: Vec<&'static str> = Vec::new();
  if is_active {
    parts.push("current tab");
  }
  if is_pinned {
    parts.push("pinned");
  }
  if loading {
    parts.push("loading");
  }
  if has_error {
    parts.push("error");
  }
  if has_warning {
    parts.push("warning");
  }
  if parts.is_empty() {
    title.to_string()
  } else {
    format!("{title} ({})", parts.join(", "))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

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
}
