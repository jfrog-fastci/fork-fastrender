//! ASCII-only case-insensitive substring search helpers.
//!
//! These routines are used in hot UI paths (omnibox scoring, visited/history searching). They are:
//! - allocation-free,
//! - ASCII-only for case folding (non-ASCII bytes must match exactly),
//! - optimized for repeated queries by requiring the needle be pre-lowercased with
//!   `to_ascii_lowercase()`.
//!
//! The API explicitly takes `needle_lower_ascii` (already ASCII-lowercased) so callers can do the
//! conversion once per query token, rather than once per `(haystack, needle)` comparison.

use memchr::{memchr, memchr2};
use std::hash::{Hash, Hasher};

/// Find the first occurrence of `needle_lower_ascii` in `haystack` using ASCII-only
/// case-insensitive matching.
///
/// - `needle_lower_ascii` **must** already be ASCII-lowercased (i.e. produced by
///   `to_ascii_lowercase()`); non-ASCII bytes are unaffected by `to_ascii_lowercase` and therefore
///   still compare exactly.
/// - Returns the byte index of the first match (compatible with `str::find` semantics).
pub(crate) fn find_ascii_case_insensitive(
  haystack: &str,
  needle_lower_ascii: &str,
) -> Option<usize> {
  if needle_lower_ascii.is_empty() {
    return Some(0);
  }

  let hay = haystack.as_bytes();
  let needle = needle_lower_ascii.as_bytes();
  if needle.len() > hay.len() {
    return None;
  }

  debug_assert!(
    needle.iter().all(|b| !b.is_ascii_uppercase()),
    "needle_lower_ascii must already be ASCII-lowercased"
  );

  let first = needle[0];
  let first_upper = if first.is_ascii_lowercase() {
    first.to_ascii_uppercase()
  } else {
    first
  };

  // 1-byte needle fast path: just scan for either `a` or `A`.
  if needle.len() == 1 {
    if first_upper != first {
      return memchr2(first, first_upper, hay);
    }
    return memchr(first, hay);
  }

  let last_start = hay.len() - needle.len();
  let mut offset = 0usize;
  while offset <= last_start {
    let rel = if first_upper != first {
      memchr2(first, first_upper, &hay[offset..])?
    } else {
      memchr(first, &hay[offset..])?
    };
    let start = offset + rel;

    // `memchr` searches the entire remainder of the slice, so it can return a match position that
    // doesn't leave enough room for the full needle. In that case, we're done.
    if start > last_start {
      return None;
    }

    if matches_at(hay, start, needle) {
      return Some(start);
    }

    offset = start + 1;
  }

  None
}

#[inline]
fn matches_at(hay: &[u8], start: usize, needle_lower: &[u8]) -> bool {
  let window = &hay[start..start + needle_lower.len()];
  for (&h, &n) in window.iter().zip(needle_lower.iter()) {
    if h == n {
      continue;
    }
    if n.is_ascii_lowercase() {
      // `n` is guaranteed to be `a..=z`, so `n - 32` is `A..=Z`.
      if h == n - 32 {
        continue;
      }
    }
    return false;
  }
  true
}

/// Returns `true` if `haystack` contains `needle_lower_ascii` using ASCII-only case-insensitive
/// matching.
#[inline]
pub(crate) fn contains_ascii_case_insensitive(haystack: &str, needle_lower_ascii: &str) -> bool {
  find_ascii_case_insensitive(haystack, needle_lower_ascii).is_some()
}

/// A `&str` wrapper with ASCII case-insensitive `Hash` + `Eq`.
///
/// Useful for allocation-free de-duplication of strings in `HashSet`/`HashMap` where the desired
/// semantics are:
/// - ASCII bytes compare case-insensitively.
/// - Non-ASCII bytes compare exactly.
#[derive(Debug, Clone, Copy)]
pub(crate) struct AsciiCaseInsensitiveStr<'a>(pub &'a str);

impl PartialEq for AsciiCaseInsensitiveStr<'_> {
  #[inline]
  fn eq(&self, other: &Self) -> bool {
    self.0.eq_ignore_ascii_case(other.0)
  }
}

impl Eq for AsciiCaseInsensitiveStr<'_> {}

impl Hash for AsciiCaseInsensitiveStr<'_> {
  #[inline]
  fn hash<H: Hasher>(&self, state: &mut H) {
    // Ensure hashing is consistent with the `Eq` implementation above: fold ASCII to lowercase and
    // hash bytes directly.
    for &b in self.0.as_bytes() {
      state.write_u8(b.to_ascii_lowercase());
    }
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn empty_needle_matches_everything() {
    assert_eq!(find_ascii_case_insensitive("abc", ""), Some(0));
    assert_eq!(find_ascii_case_insensitive("", ""), Some(0));
    assert!(contains_ascii_case_insensitive("abc", ""));
    assert!(contains_ascii_case_insensitive("", ""));
  }

  #[test]
  fn ascii_mixed_case_matches() {
    assert_eq!(find_ascii_case_insensitive("HelloWorld", "world"), Some(5));
    assert!(contains_ascii_case_insensitive("HelloWorld", "world"));

    assert_eq!(find_ascii_case_insensitive("HELLO", "hello"), Some(0));
    assert_eq!(find_ascii_case_insensitive("heLLo", "ell"), Some(1));

    assert_eq!(find_ascii_case_insensitive("abc", "d"), None);
    assert!(!contains_ascii_case_insensitive("abc", "d"));
  }

  #[test]
  fn non_ascii_bytes_compare_exactly() {
    // ASCII case folding should still work for the ASCII prefix.
    assert!(!contains_ascii_case_insensitive("CAFÉ", "café"));

    // Exact byte match on non-ASCII.
    assert!(contains_ascii_case_insensitive("café", "fé"));
    assert!(contains_ascii_case_insensitive("É", "É"));
    assert!(!contains_ascii_case_insensitive("É", "é"));
  }

  #[test]
  fn does_not_assume_utf8_boundaries() {
    // `€` is a 3-byte UTF-8 sequence. Matching against ASCII shouldn't panic even though the
    // candidate window can end on a non-UTF8 boundary.
    assert_eq!(find_ascii_case_insensitive("a€b", "ab"), None);
    assert_eq!(find_ascii_case_insensitive("€Hello", "hello"), Some(3));
  }

  #[test]
  fn token_matching_matches_any_field_and_ands_tokens() {
    // Mirrors the `about:` / history-panel call pattern:
    // - query is lowercased once
    // - tokens are ANDed
    // - each token may match either URL or title
    let query_lower = "RUST programming".to_ascii_lowercase();
    let tokens: Vec<&str> = query_lower.split_whitespace().collect();

    // `rust` matches the URL only; `programming` matches the title only.
    let url = "https://example.com/RuStacean";
    let title = Some("Systems PROGRAMMING guide");
    assert!(tokens.iter().all(|t| {
      contains_ascii_case_insensitive(url, t)
        || title.is_some_and(|title| contains_ascii_case_insensitive(title, t))
    }));

    // If the title is missing, the `programming` token should fail to match.
    let title_missing: Option<&str> = None;
    assert!(!tokens.iter().all(|t| {
      contains_ascii_case_insensitive(url, t)
        || title_missing.is_some_and(|title| contains_ascii_case_insensitive(title, t))
    }));

    // A title that's present but doesn't match should also fail.
    let title_wrong = Some("Rustacean guide");
    assert!(!tokens.iter().all(|t| {
      contains_ascii_case_insensitive(url, t)
        || title_wrong.is_some_and(|title| contains_ascii_case_insensitive(title, t))
    }));
  }
}
