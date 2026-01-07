//! Lightweight memory sampling for diagnostics and guardrails.
//!
//! Currently this module focuses on Linux where `/proc/self/status` provides a stable RSS
//! (resident set size) snapshot without external dependencies.

/// Returns the current process RSS (resident set size) in bytes when available.
///
/// This is best-effort and must never panic: if sampling is unsupported or parsing fails, this
/// returns `None`.
pub fn current_rss_bytes() -> Option<u64> {
  #[cfg(target_os = "linux")]
  {
    let contents = std::fs::read("/proc/self/status").ok()?;
    let rss_kb = parse_proc_status_kb(&contents, b"VmRSS:")?;
    return rss_kb.checked_mul(1024);
  }

  #[cfg(not(target_os = "linux"))]
  {
    None
  }
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
      value = value
        .checked_mul(10)?
        .checked_add(u64::from(byte - b'0'))?;
      idx += 1;
    }
    return Some(value);
  }

  None
}

#[cfg(test)]
mod tests {
  use super::*;

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
  fn parse_proc_status_rejects_malformed_lines_without_panicking() {
    assert_eq!(parse_proc_status_kb(b"VmRSS:\t kB\n", b"VmRSS:"), None);
    assert_eq!(parse_proc_status_kb(b"VmRSS:\t abc kB\n", b"VmRSS:"), None);
    // Overflow should return None (not panic).
    let huge = b"VmRSS:\t184467440737095516160 kB\n";
    assert_eq!(parse_proc_status_kb(huge, b"VmRSS:"), None);
  }
}

