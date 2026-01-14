use crate::ui::downloads::sanitize_download_filename;
use crate::ui::BrowserTabState;

const MAX_SUGGESTED_FILENAME_BYTES: usize = 120;

fn raw_is_effectively_empty(raw: &str) -> bool {
  raw.chars().all(|c| matches!(c, '/' | '\\' | '.' | ' ') || c.is_control())
}

fn strip_extension_ignore_case<'a>(name: &'a str, ext: &str) -> &'a str {
  let ext = ext.strip_prefix('.').unwrap_or(ext);
  let suffix = format!(".{ext}");
  if let Some(tail) = name.get(name.len().saturating_sub(suffix.len())..) {
    if tail.eq_ignore_ascii_case(&suffix) {
      return &name[..name.len() - suffix.len()];
    }
  }
  name
}

fn url_basename(url: &str) -> Option<String> {
  let parsed = url::Url::parse(url).ok()?;

  if parsed.scheme() == "file" {
    let path = parsed.to_file_path().ok()?;
    return path
      .file_name()
      .map(|name| name.to_string_lossy().to_string());
  }

  if let Some(host) = parsed.host_str() {
    let seg = parsed
      .path_segments()
      .and_then(|segments| segments.filter(|seg| !seg.is_empty()).last());
    return Some(match seg {
      Some(seg) => format!("{host}_{seg}"),
      None => host.to_string(),
    });
  }

  let scheme = parsed.scheme();
  let seg = parsed
    .path_segments()
    .and_then(|segments| segments.filter(|seg| !seg.is_empty()).last());
  if let Some(seg) = seg {
    return Some(format!("{scheme}_{seg}"));
  }

  let path = parsed.path().trim_matches('/');
  if !path.is_empty() {
    return Some(format!("{scheme}_{path}"));
  }

  Some(scheme.to_string())
}

fn ensure_extension_lowercase(name: &str, ext: &str) -> String {
  let ext = ext.strip_prefix('.').unwrap_or(ext);
  let suffix = format!(".{ext}");
  if let Some(tail) = name.get(name.len().saturating_sub(suffix.len())..) {
    if !tail.eq_ignore_ascii_case(&suffix) {
      return format!("{name}{suffix}");
    }
    let stem = &name[..name.len() - suffix.len()];
    format!("{stem}{suffix}")
  } else {
    format!("{name}{suffix}")
  }
}

fn suggested_filename_with_ext(tab: &BrowserTabState, ext: &str) -> String {
  let title_candidate = tab
    .committed_title
    .as_deref()
    .map(str::trim)
    .filter(|title| !title.is_empty())
    .filter(|title| !raw_is_effectively_empty(title));

  let url_candidate = tab
    .committed_url
    .as_deref()
    .or(tab.current_url.as_deref())
    .and_then(url_basename)
    .map(|name| name.trim().to_string())
    .filter(|name| !name.is_empty())
    .filter(|name| !raw_is_effectively_empty(name));

  let raw = title_candidate
    .map(str::to_string)
    .or(url_candidate)
    .unwrap_or_else(|| "page".to_string());

  let sanitized = sanitize_download_filename(&raw);
  let stem = strip_extension_ignore_case(&sanitized, ext);
  let stem = crate::ui::clipboard::truncate_utf8_to_max_bytes(stem, MAX_SUGGESTED_FILENAME_BYTES);

  // Truncation can re-introduce a Windows-incompatible trailing dot/space. Trim those again.
  let mut stem = stem.to_string();
  while stem.ends_with('.') || stem.ends_with(' ') {
    stem.pop();
  }
  if stem.trim().is_empty() {
    stem = "page".to_string();
  }

  // Re-sanitize after truncation to preserve Windows reserved device name handling.
  let stem = sanitize_download_filename(&stem);
  ensure_extension_lowercase(&stem, ext)
}

/// Suggested default filename for "Save Page…", ending in `.html`.
pub fn suggested_save_page_filename(tab: &BrowserTabState) -> String {
  suggested_filename_with_ext(tab, "html")
}

/// Suggested default filename for "Print…", ending in `.png`.
pub fn suggested_print_filename(tab: &BrowserTabState) -> String {
  suggested_filename_with_ext(tab, "png")
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::ui::TabId;

  #[test]
  fn whitespace_title_falls_back_to_url() {
    let mut tab = BrowserTabState::new(TabId(1), "https://example.com/dir/page".to_string());
    tab.committed_title = Some("   ".to_string());
    assert_eq!(
      suggested_save_page_filename(&tab),
      "example.com_page.html".to_string()
    );
    assert_eq!(
      suggested_print_filename(&tab),
      "example.com_page.png".to_string()
    );
  }

  #[test]
  fn title_is_sanitized_for_cross_platform_filenames() {
    let mut tab = BrowserTabState::new(TabId(1), "https://example.com".to_string());
    tab.committed_title = Some("a/b:c*".to_string());
    assert_eq!(suggested_save_page_filename(&tab), "ab_c_.html".to_string());
    assert_eq!(suggested_print_filename(&tab), "ab_c_.png".to_string());
  }

  #[test]
  fn url_with_no_path_segment_uses_host_only() {
    let tab = BrowserTabState::new(TabId(1), "https://example.com".to_string());
    assert_eq!(suggested_save_page_filename(&tab), "example.com.html".to_string());
    assert_eq!(suggested_print_filename(&tab), "example.com.png".to_string());
  }

  #[test]
  fn avoids_reserved_windows_device_names() {
    let mut tab = BrowserTabState::new(TabId(1), "https://example.com".to_string());
    tab.committed_title = Some("CON".to_string());
    assert_eq!(suggested_save_page_filename(&tab), "_CON.html".to_string());
    assert_eq!(suggested_print_filename(&tab), "_CON.png".to_string());
  }
}
