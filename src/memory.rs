//! Lightweight memory sampling for diagnostics and guardrails.
//!
//! Currently this module focuses on Linux where `/proc/self/status` provides a stable RSS
//! (resident set size) snapshot without external dependencies.
//!
//! Note: Some callers (e.g. the headless UI perf harness) sample RSS periodically to track
//! `rss_bytes_peak`. Prefer the lighter-weight `/proc/self/statm` when available to avoid
//! allocating/parsing a multi-KiB status file on every sample.

/// Bytes per MiB (mebibyte, 1024×1024).
pub const BYTES_PER_MIB: u64 = 1024 * 1024;

/// Returns the current process RSS (resident set size) in bytes when available.
///
/// This is best-effort and must never panic: if sampling is unsupported or parsing fails, this
/// returns `None`.
pub fn current_rss_bytes() -> Option<u64> {
  #[cfg(target_os = "linux")]
  {
    if let Some(rss_bytes) = current_rss_bytes_from_proc_statm() {
      return Some(rss_bytes);
    }

    let contents = std::fs::read("/proc/self/status").ok()?;
    let rss_kb = parse_proc_status_kb(&contents, b"VmRSS:")?;
    return rss_kb.checked_mul(1024);
  }

  #[cfg(not(target_os = "linux"))]
  {
    None
  }
}

/// Convert a byte count to megabytes (MiB) as a floating-point value.
#[inline]
pub fn bytes_to_mb(bytes: u64) -> f64 {
  bytes as f64 / BYTES_PER_MIB as f64
}

#[cfg(target_os = "linux")]
fn current_rss_bytes_from_proc_statm() -> Option<u64> {
  let contents = std::fs::read("/proc/self/statm").ok()?;
  let resident_pages = parse_proc_statm_resident_pages(&contents)?;
  resident_pages.checked_mul(page_size_bytes()?)
}

#[cfg(target_os = "linux")]
fn page_size_bytes() -> Option<u64> {
  use std::sync::OnceLock;

  static PAGE_SIZE: OnceLock<Option<u64>> = OnceLock::new();
  *PAGE_SIZE.get_or_init(|| {
    let size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if size <= 0 {
      None
    } else {
      Some(size as u64)
    }
  })
}

#[cfg(target_os = "linux")]
fn parse_proc_statm_resident_pages(contents: &[u8]) -> Option<u64> {
  // Expected format:
  //   size resident share text lib data dt
  // where each field is a decimal number of pages.
  //
  // We only need the second field (resident).
  let mut idx = 0;

  // Skip leading whitespace.
  while idx < contents.len() && contents[idx].is_ascii_whitespace() {
    idx += 1;
  }
  if idx >= contents.len() || !contents[idx].is_ascii_digit() {
    return None;
  }

  // Parse first number (size) and ignore.
  while idx < contents.len() && contents[idx].is_ascii_digit() {
    idx += 1;
  }

  // Skip whitespace between fields.
  while idx < contents.len() && contents[idx].is_ascii_whitespace() {
    idx += 1;
  }
  if idx >= contents.len() || !contents[idx].is_ascii_digit() {
    return None;
  }

  let mut value: u64 = 0;
  while idx < contents.len() {
    let byte = contents[idx];
    if !byte.is_ascii_digit() {
      break;
    }
    value = value.checked_mul(10)?.checked_add(u64::from(byte - b'0'))?;
    idx += 1;
  }
  Some(value)
}

#[cfg(target_os = "linux")]
fn parse_proc_status_kb(contents: &[u8], key: &[u8]) -> Option<u64> {
  // `/proc/self/status` is small (< 8 KiB) and line-oriented. Walk it without allocating.
  for line in contents.split(|b| *b == b'\n') {
    if !line.starts_with(key) {
      continue;
    }

    // Expected format: `VmRSS:\t   12345 kB`.
    let mut idx = key.len();
    while idx < line.len() && (line[idx] == b' ' || line[idx] == b'\t') {
      idx += 1;
    }
    if idx >= line.len() || !line[idx].is_ascii_digit() {
      return None;
    }

    let mut value: u64 = 0;
    while idx < line.len() {
      let byte = line[idx];
      if !byte.is_ascii_digit() {
        break;
      }
      value = value.checked_mul(10)?.checked_add(u64::from(byte - b'0'))?;
      idx += 1;
    }
    return Some(value);
  }

  None
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn bytes_to_mb_is_stable() {
    assert_eq!(bytes_to_mb(0), 0.0);
    assert_eq!(bytes_to_mb(BYTES_PER_MIB), 1.0);
  }

  #[cfg(target_os = "linux")]
  #[test]
  fn parse_proc_status_handles_empty_and_missing_inputs() {
    assert_eq!(parse_proc_status_kb(b"", b"VmRSS:"), None);
    assert_eq!(
      parse_proc_status_kb(b"Name:\ttest\nVmSize:\t1 kB\n", b"VmRSS:"),
      None
    );
  }

  #[cfg(target_os = "linux")]
  #[test]
  fn parse_proc_status_parses_rss_kb() {
    let contents = b"Name:\ttest\nVmRSS:\t   12345 kB\n";
    assert_eq!(parse_proc_status_kb(contents, b"VmRSS:"), Some(12345));
  }

  #[cfg(target_os = "linux")]
  #[test]
  fn parse_proc_statm_parses_resident_pages() {
    let contents = b"123 456 789 0 0 0 0\n";
    assert_eq!(parse_proc_statm_resident_pages(contents), Some(456));
  }

  #[cfg(target_os = "linux")]
  #[test]
  fn parse_proc_statm_rejects_malformed_inputs() {
    assert_eq!(parse_proc_statm_resident_pages(b""), None);
    assert_eq!(parse_proc_statm_resident_pages(b"123\n"), None);
    assert_eq!(parse_proc_statm_resident_pages(b"abc def\n"), None);
    // Overflow should return None.
    let huge = b"1 184467440737095516160 3\n";
    assert_eq!(parse_proc_statm_resident_pages(huge), None);
  }

  #[cfg(target_os = "linux")]
  #[test]
  fn parse_proc_status_rejects_malformed_lines_without_panicking() {
    assert_eq!(parse_proc_status_kb(b"VmRSS:\t kB\n", b"VmRSS:"), None);
    assert_eq!(parse_proc_status_kb(b"VmRSS:\t abc kB\n", b"VmRSS:"), None);
    // Overflow should return None (not panic).
    let huge = b"VmRSS:\t184467440737095516160 kB\n";
    assert_eq!(parse_proc_status_kb(huge, b"VmRSS:"), None);
  }
}
