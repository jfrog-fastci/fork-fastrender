/// Pure helpers for formatting/truncating URLs for display in the browser chrome.
///
/// This module intentionally does **not** attempt full URL parsing/normalization. It operates on
/// the raw string shown in the address bar and aims to provide stable, deterministic truncation for
/// UI display.
///
/// The main goal is to produce a *middle-ellipsis* representation for long URLs, preferably keeping
/// the scheme+host prefix and the last path segment (e.g. `https://example.com/…/path`).
use memchr::memchr3;
use std::borrow::Cow;

pub fn truncate_url_middle_cow(url: &str, max_chars: usize) -> Cow<'_, str> {
  if max_chars == 0 {
    return Cow::Borrowed("");
  }

  if url.chars().count() <= max_chars {
    return Cow::Borrowed(url);
  }

  // Prefer a structured `prefix/…/tail` truncation for hierarchical URLs of the form
  // `scheme://authority/path`.
  if let Some(scheme_sep) = url.find("://") {
    let after_authority_start = scheme_sep + "://".len();
    let after = &url[after_authority_start..];
    let authority_rel_end = memchr3(b'/', b'?', b'#', after.as_bytes()).unwrap_or(after.len());
    let authority_end = after_authority_start + authority_rel_end;
    let prefix = &url[..authority_end];
    let remainder = &url[authority_end..];

    if !remainder.is_empty() {
      if let Some(last_slash) = remainder.rfind('/') {
        // Avoid the `prefix/…/` output for root-only URLs.
        let tail = &remainder[last_slash..];
        if tail != "/" {
          let candidate = format!("{prefix}/…{tail}");
          if candidate.chars().count() <= max_chars {
            return Cow::Owned(candidate);
          }
        }
      }
    }
  }

  Cow::Owned(truncate_middle(url, max_chars))
}

pub fn truncate_url_middle(url: &str, max_chars: usize) -> String {
  truncate_url_middle_cow(url, max_chars).into_owned()
}

fn truncate_middle(value: &str, max_chars: usize) -> String {
  if max_chars == 0 {
    return String::new();
  }
  let len = value.chars().count();
  if len <= max_chars {
    return value.to_string();
  }
  if max_chars == 1 {
    return "…".to_string();
  }

  let keep_start = (max_chars - 1) / 2;
  let keep_end = max_chars - 1 - keep_start;

  let mut start = String::new();
  start.extend(value.chars().take(keep_start));

  let mut end_rev = String::new();
  end_rev.extend(value.chars().rev().take(keep_end));
  let end = end_rev.chars().rev().collect::<String>();

  format!("{start}…{end}")
}

#[cfg(test)]
mod tests {
  use super::truncate_url_middle;

  #[test]
  fn short_urls_are_unchanged() {
    let url = "https://example.com/path";
    assert_eq!(truncate_url_middle(url, 200), url);
  }

  #[test]
  fn prefers_prefix_and_last_path_segment_when_it_fits() {
    let url = "https://example.com/a/b/path";
    assert_eq!(truncate_url_middle(url, 26), "https://example.com/…/path");
  }

  #[test]
  fn handles_unicode_paths() {
    let url = "https://example.com/über/路径/文件.html";
    assert_eq!(
      truncate_url_middle(url, 29),
      "https://example.com/…/文件.html"
    );
  }

  #[test]
  fn handles_file_urls() {
    let url = "file:///Users/alice/Documents/report.html";
    assert_eq!(truncate_url_middle(url, 32), "file:///…/report.html");
  }

  #[test]
  fn very_long_domains_fall_back_to_generic_middle_ellipsis() {
    let url = "https://this-is-a-very-long-domain-name.example.com/path";
    let truncated = truncate_url_middle(url, 30);
    assert_eq!(truncated.chars().count(), 30);
    assert!(truncated.contains('…'));
    assert!(truncated.starts_with("https://"));
    assert!(truncated.ends_with("/path"));
  }

  #[test]
  fn no_path_falls_back_to_generic_middle_ellipsis() {
    let url = "https://example.com/";
    let truncated = truncate_url_middle(url, 10);
    assert_eq!(truncated.chars().count(), 10);
    assert!(truncated.contains('…'));
    assert!(truncated.starts_with("http"));
    assert!(truncated.ends_with("com/"));
  }
}
