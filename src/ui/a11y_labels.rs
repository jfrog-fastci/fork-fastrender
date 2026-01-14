//! Shared accessibility labels for egui widgets.
//!
//! Many browser UI panels render lists of entries where each row has the same action buttons
//! (e.g. "Open", "Delete", "Retry"). Without additional context, screen readers will announce
//! dozens of indistinguishable buttons.
//!
//! This module generates contextual labels that incorporate the relevant entry title/URL/file name
//! so each action is uniquely understandable.

fn normalize_context_title_or_url(title: Option<&str>, url: &str) -> String {
  let title = title.unwrap_or("").trim();
  let url = url.trim();

  let raw = if !title.is_empty() { title } else { url };

  // Fast path: the common case is an ASCII URL with no whitespace. Avoid split/loop overhead.
  if raw
    .as_bytes()
    .iter()
    .all(|&b| b.is_ascii() && !b.is_ascii_whitespace())
  {
    return raw.to_string();
  }

  // Strip newlines/tabs and collapse whitespace so screen readers get a concise name.
  let mut out = String::with_capacity(raw.len());
  for (idx, part) in raw.split_whitespace().enumerate() {
    if idx > 0 {
      out.push(' ');
    }
    out.push_str(part);
  }
  out
}

fn normalize_file_name(file_name: &str) -> String {
  let raw = file_name.trim();
  // Fast path: most file names are ASCII and whitespace-free.
  if raw
    .as_bytes()
    .iter()
    .all(|&b| b.is_ascii() && !b.is_ascii_whitespace())
  {
    return raw.to_string();
  }
  let mut out = String::with_capacity(raw.len());
  for (idx, part) in raw.split_whitespace().enumerate() {
    if idx > 0 {
      out.push(' ');
    }
    out.push_str(part);
  }
  out
}

pub fn history_open_label(title: Option<&str>, url: &str) -> String {
  let ctx = normalize_context_title_or_url(title, url);
  format!("Open history entry: {ctx}")
}

pub fn history_open_in_new_tab_label(title: Option<&str>, url: &str) -> String {
  let ctx = normalize_context_title_or_url(title, url);
  format!("Open history entry in new tab: {ctx}")
}

pub fn history_delete_label(title: Option<&str>, url: &str) -> String {
  let ctx = normalize_context_title_or_url(title, url);
  format!("Delete history entry: {ctx}")
}

pub fn download_cancel_label(file_name: &str) -> String {
  let file_name = normalize_file_name(file_name);
  format!("Cancel download: {file_name}")
}

pub fn download_open_label(file_name: &str) -> String {
  let file_name = normalize_file_name(file_name);
  format!("Open downloaded file: {file_name}")
}

pub fn download_show_in_folder_label(file_name: &str) -> String {
  let file_name = normalize_file_name(file_name);
  format!("Show {file_name} in folder")
}

pub fn download_retry_label(file_name: &str) -> String {
  let file_name = normalize_file_name(file_name);
  format!("Retry download: {file_name}")
}

pub fn download_copy_link_label(file_name: &str) -> String {
  let file_name = normalize_file_name(file_name);
  format!("Copy download link: {file_name}")
}

pub fn download_copy_path_label(file_name: &str) -> String {
  let file_name = normalize_file_name(file_name);
  format!("Copy download file path: {file_name}")
}

pub fn bookmark_open_in_new_tab_label(title: Option<&str>, url: &str) -> String {
  let ctx = normalize_context_title_or_url(title, url);
  format!("Open bookmark in new tab: {ctx}")
}

pub fn bookmark_edit_label(title: Option<&str>, url: &str) -> String {
  let ctx = normalize_context_title_or_url(title, url);
  format!("Edit bookmark: {ctx}")
}

pub fn bookmark_delete_label(title: Option<&str>, url: &str) -> String {
  let ctx = normalize_context_title_or_url(title, url);
  format!("Delete bookmark: {ctx}")
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn normalize_context_prefers_title() {
    assert_eq!(
      normalize_context_title_or_url(Some(" Example title "), "https://example.com"),
      "Example title"
    );
  }

  #[test]
  fn normalize_context_falls_back_to_url_when_title_missing() {
    assert_eq!(
      normalize_context_title_or_url(Some("   "), " https://example.com/test "),
      "https://example.com/test"
    );
  }

  #[test]
  fn normalize_context_collapses_whitespace() {
    assert_eq!(
      normalize_context_title_or_url(Some("Hello\nworld\t!"), "https://example.com"),
      "Hello world !"
    );
  }

  #[test]
  fn history_labels_include_context() {
    assert_eq!(
      history_open_label(Some("Example"), "https://example.com"),
      "Open history entry: Example"
    );
    assert_eq!(
      history_open_in_new_tab_label(None, "https://example.com"),
      "Open history entry in new tab: https://example.com"
    );
    assert_eq!(
      history_delete_label(Some("Example"), "https://example.com"),
      "Delete history entry: Example"
    );
  }

  #[test]
  fn download_labels_include_file_name() {
    assert_eq!(
      download_cancel_label("file.zip"),
      "Cancel download: file.zip"
    );
    assert_eq!(
      download_open_label("file.zip"),
      "Open downloaded file: file.zip"
    );
    assert_eq!(
      download_show_in_folder_label("file.zip"),
      "Show file.zip in folder"
    );
    assert_eq!(download_retry_label("file.zip"), "Retry download: file.zip");
    assert_eq!(
      download_copy_link_label("file.zip"),
      "Copy download link: file.zip"
    );
    assert_eq!(
      download_copy_path_label("file.zip"),
      "Copy download file path: file.zip"
    );
  }

  #[test]
  fn bookmark_labels_include_context() {
    assert_eq!(
      bookmark_open_in_new_tab_label(Some("Rust"), "https://www.rust-lang.org"),
      "Open bookmark in new tab: Rust"
    );
    assert_eq!(
      bookmark_edit_label(None, "https://example.com"),
      "Edit bookmark: https://example.com"
    );
    assert_eq!(
      bookmark_delete_label(Some("Example"), "https://example.com"),
      "Delete bookmark: Example"
    );
  }
}
