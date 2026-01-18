//! ASCII-only case-insensitive substring search helpers.
//!
//! These routines are used in hot UI paths (omnibox scoring, visited/history searching) and some
//! renderer-only parsing utilities (e.g. `<meta http-equiv=refresh>` heuristics). They are:
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
/// case-insensitive matching, starting at `start`.
///
/// - `needle_lower_ascii` **must** already be ASCII-lowercased (i.e. produced by
///   `to_ascii_lowercase()`); non-ASCII bytes are unaffected by `to_ascii_lowercase` and therefore
///   still compare exactly.
/// - Returns the byte index of the first match (compatible with `str::find` semantics when
///   `start == 0`).
pub(crate) fn find_ascii_case_insensitive_bytes_from(
  haystack: &[u8],
  needle_lower_ascii: &[u8],
  start: usize,
) -> Option<usize> {
  if needle_lower_ascii.is_empty() {
    return (start <= haystack.len()).then_some(start);
  }
  if start >= haystack.len() {
    return None;
  }

  if needle_lower_ascii.len() > haystack.len() - start {
    return None;
  }

  debug_assert!(
    needle_lower_ascii.iter().all(|b| !b.is_ascii_uppercase()),
    "needle_lower_ascii must already be ASCII-lowercased"
  );

  let first = needle_lower_ascii[0];
  let first_upper = if first.is_ascii_lowercase() {
    first.to_ascii_uppercase()
  } else {
    first
  };

  // 1-byte needle fast path: just scan for either `a` or `A`.
  if needle_lower_ascii.len() == 1 {
    let rel = if first_upper != first {
      memchr2(first, first_upper, &haystack[start..])
    } else {
      memchr(first, &haystack[start..])
    }?;
    return Some(start + rel);
  }

  let last_start = haystack.len() - needle_lower_ascii.len();
  let mut offset = start;
  while offset <= last_start {
    let rel = if first_upper != first {
      memchr2(first, first_upper, &haystack[offset..])?
    } else {
      memchr(first, &haystack[offset..])?
    };
    let start = offset + rel;

    // `memchr` searches the entire remainder of the slice, so it can return a match position that
    // doesn't leave enough room for the full needle. In that case, we're done.
    if start > last_start {
      return None;
    }

    if matches_at(haystack, start, needle_lower_ascii) {
      return Some(start);
    }

    offset = start + 1;
  }

  None
}

/// Find the first occurrence of `needle_lower_ascii` in `haystack` using ASCII-only
/// case-insensitive matching.
pub(crate) fn find_ascii_case_insensitive_bytes(
  haystack: &[u8],
  needle_lower_ascii: &[u8],
) -> Option<usize> {
  find_ascii_case_insensitive_bytes_from(haystack, needle_lower_ascii, 0)
}

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
  find_ascii_case_insensitive_bytes(haystack.as_bytes(), needle_lower_ascii.as_bytes())
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
pub(crate) struct AsciiCaseInsensitive<'a>(pub &'a str);

// Some call sites (and tests) prefer the more explicit name; keep it as a rename so the tuple
// struct constructor is also available (`AsciiCaseInsensitiveStr("...")`).
pub(crate) use AsciiCaseInsensitive as AsciiCaseInsensitiveStr;

impl PartialEq for AsciiCaseInsensitive<'_> {
  #[inline]
  fn eq(&self, other: &Self) -> bool {
    self.0.eq_ignore_ascii_case(other.0)
  }
}

impl Eq for AsciiCaseInsensitive<'_> {}

impl Hash for AsciiCaseInsensitive<'_> {
  #[inline]
  fn hash<H: Hasher>(&self, state: &mut H) {
    // Ensure hashing is consistent with the `Eq` implementation above: fold ASCII to lowercase and
    // hash bytes directly.
    //
    // NOTE: Avoid calling `Hasher::write_u8` in a tight loop, which can be surprisingly expensive
    // for many hashers because the default implementation forwards to `write(&[u8])` per byte.
    const BUF_LEN: usize = 64;
    let mut buf = [0u8; BUF_LEN];
    let mut len = 0usize;
    for &b in self.0.as_bytes() {
      buf[len] = b.to_ascii_lowercase();
      len += 1;
      if len == BUF_LEN {
        state.write(&buf);
        len = 0;
      }
    }
    if len != 0 {
      state.write(&buf[..len]);
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

  #[test]
  fn bytes_from_start_offset_respects_start() {
    let hay = b"HelloHello";
    let needle = b"hello";

    assert_eq!(
      find_ascii_case_insensitive_bytes_from(hay, needle, 0),
      Some(0)
    );
    assert_eq!(
      find_ascii_case_insensitive_bytes_from(hay, needle, 1),
      Some(5)
    );
    assert_eq!(find_ascii_case_insensitive_bytes_from(hay, needle, 6), None);

    // Empty needle matches at the provided start index.
    assert_eq!(find_ascii_case_insensitive_bytes_from(hay, b"", 3), Some(3));
    assert_eq!(
      find_ascii_case_insensitive_bytes_from(hay, b"", hay.len()),
      Some(hay.len())
    );
  }

  #[test]
  fn ascii_case_insensitive_hash_and_eq() {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::Hasher;

    fn hash<T: Hash>(value: &T) -> u64 {
      let mut hasher = DefaultHasher::new();
      value.hash(&mut hasher);
      hasher.finish()
    }

    let a = AsciiCaseInsensitive("HTTP://EXAMPLE.COM");
    let b = AsciiCaseInsensitive("http://example.com");
    assert_eq!(a, b);
    assert_eq!(hash(&a), hash(&b));

    // Non-ASCII bytes must compare exactly (ASCII-only case folding policy).
    assert_ne!(AsciiCaseInsensitive("café"), AsciiCaseInsensitive("cafÉ"));
  }

  #[test]
  fn ascii_case_insensitive_constructor_is_usable() {
    use std::collections::HashSet;

    let mut set: HashSet<AsciiCaseInsensitive<'_>> = HashSet::new();
    assert!(set.insert(AsciiCaseInsensitive("Hello")));
    assert!(!set.insert(AsciiCaseInsensitive("hELLo")));
    assert_eq!(set.len(), 1);
  }
}
