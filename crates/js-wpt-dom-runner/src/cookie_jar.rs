use std::collections::BTreeMap;

/// Maximum number of cookies stored for a single document.
///
/// This is an intentionally small, deterministic bound for the runner; it is not intended to match
/// any particular browser limit.
pub(crate) const MAX_COOKIES_PER_DOCUMENT: usize = 128;

/// Maximum total byte length of the `document.cookie` getter result.
pub(crate) const MAX_COOKIE_STRING_BYTES: usize = 4096;

/// Minimal, deterministic in-memory cookie store used to back `document.cookie`.
///
/// MVP semantics:
/// - Stores `name=value` pairs only (attributes are ignored).
/// - Deterministic ordering: cookies are kept sorted by name.
/// - Deterministic bounds: ignores writes that would exceed cookie count or total string size.
#[derive(Debug, Default, Clone)]
pub(crate) struct CookieJar {
  cookies: BTreeMap<String, String>,
  cookie_string_len: usize,
}

impl CookieJar {
  pub(crate) fn new() -> Self {
    Self::default()
  }

  pub(crate) fn cookie_string(&self) -> String {
    if self.cookies.is_empty() {
      return String::new();
    }
    let mut out = String::with_capacity(self.cookie_string_len);
    for (i, (name, value)) in self.cookies.iter().enumerate() {
      if i > 0 {
        out.push_str("; ");
      }
      out.push_str(name);
      out.push('=');
      out.push_str(value);
    }
    out
  }

  pub(crate) fn set_cookie_string(&mut self, cookie_string: &str) {
    let Some((name, value)) = parse_set_cookie(cookie_string) else {
      return;
    };

    let pair_len = name.len() + 1 + value.len();

    if let Some(existing_value) = self.cookies.get(name) {
      let existing_pair_len = name.len() + 1 + existing_value.len();
      let new_len = self
        .cookie_string_len
        .saturating_sub(existing_pair_len)
        .saturating_add(pair_len);

      if new_len > MAX_COOKIE_STRING_BYTES {
        return;
      }

      if existing_value.as_str() == value {
        return;
      }

      self.cookies.insert(name.to_string(), value.to_string());
      self.cookie_string_len = new_len;
      return;
    }

    if self.cookies.len() >= MAX_COOKIES_PER_DOCUMENT {
      return;
    }

    let sep_len = if self.cookies.is_empty() { 0 } else { 2 };
    let new_len = self
      .cookie_string_len
      .saturating_add(sep_len)
      .saturating_add(pair_len);

    if new_len > MAX_COOKIE_STRING_BYTES {
      return;
    }

    self.cookies.insert(name.to_string(), value.to_string());
    self.cookie_string_len = new_len;
  }
}

fn parse_set_cookie(input: &str) -> Option<(&str, &str)> {
  // Cookie setter accepts an RFC6265-like cookie-string, but for our MVP we only keep the leading
  // `name=value` pair and ignore all attributes.
  let first = input.split_once(';').map(|(a, _)| a).unwrap_or(input);
  let first = first.trim_matches(|c: char| c.is_ascii_whitespace());
  let (name, value) = first.split_once('=')?;
  let name = name.trim_matches(|c: char| c.is_ascii_whitespace());
  if name.is_empty() {
    return None;
  }
  Some((name, value))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn cookie_jar_ignores_attributes_and_joins_deterministically() {
    let mut jar = CookieJar::new();
    assert_eq!(jar.cookie_string(), "");

    jar.set_cookie_string("b=c; Path=/");
    jar.set_cookie_string("a=b");
    assert_eq!(jar.cookie_string(), "a=b; b=c");
  }

  #[test]
  fn cookie_jar_enforces_total_size_limit() {
    let mut jar = CookieJar::new();
    jar.set_cookie_string("a=b");
    let too_big = format!("huge={}", "x".repeat(MAX_COOKIE_STRING_BYTES * 2));
    jar.set_cookie_string(&too_big);
    assert_eq!(jar.cookie_string(), "a=b");
  }

  #[test]
  fn cookie_jar_enforces_cookie_count_limit() {
    let mut jar = CookieJar::new();
    for i in 0..(MAX_COOKIES_PER_DOCUMENT + 10) {
      jar.set_cookie_string(&format!("k{i}=v{i}"));
    }
    assert_eq!(jar.cookies.len(), MAX_COOKIES_PER_DOCUMENT);
  }
}
