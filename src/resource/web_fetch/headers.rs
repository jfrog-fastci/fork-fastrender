use super::{Result, WebFetchError, WebFetchLimitKind, WebFetchLimits};
use http::header::HeaderName;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadersGuard {
  Immutable,
  Request,
  RequestNoCors,
  Response,
  None,
}

#[derive(Debug, Clone)]
struct Header {
  name: HeaderName,
  value: String,
}

/// An implementation of Fetch's `Headers` class.
///
/// This stores the underlying "header list" as an ordered list of pairs and supports duplicates.
#[derive(Debug, Clone)]
pub struct Headers {
  header_list: Vec<Header>,
  guard: HeadersGuard,
  limits: WebFetchLimits,
}

impl Default for Headers {
  fn default() -> Self {
    Self::new()
  }
}

impl Headers {
  pub fn new() -> Self {
    Self {
      header_list: Vec::new(),
      guard: HeadersGuard::None,
      limits: WebFetchLimits::default(),
    }
  }

  pub fn new_with_limits(limits: &WebFetchLimits) -> Self {
    Self {
      header_list: Vec::new(),
      guard: HeadersGuard::None,
      limits: limits.clone(),
    }
  }

  pub fn new_with_guard(guard: HeadersGuard) -> Self {
    Self {
      header_list: Vec::new(),
      guard,
      limits: WebFetchLimits::default(),
    }
  }

  pub fn new_with_guard_and_limits(guard: HeadersGuard, limits: &WebFetchLimits) -> Self {
    Self {
      header_list: Vec::new(),
      guard,
      limits: limits.clone(),
    }
  }

  pub fn guard(&self) -> HeadersGuard {
    self.guard
  }

  pub fn limits(&self) -> &WebFetchLimits {
    &self.limits
  }

  pub fn set_guard(&mut self, guard: HeadersGuard) {
    self.guard = guard;
  }

  pub fn append(&mut self, name: &str, value: &str) -> Result<()> {
    let name = validate_header_name(name)?;
    let value = normalize_header_value(value);
    validate_header_value(value)?;

    if !self.validate_mutation(&name, value)? {
      return Ok(());
    }

    if self.guard == HeadersGuard::RequestNoCors {
      let mut temporary_value = self.header_list_get(&name).unwrap_or_default();
      if temporary_value.is_empty() {
        temporary_value = value.to_string();
      } else {
        temporary_value.push_str(", ");
        temporary_value.push_str(value);
      }

      if !is_no_cors_safelisted_request_header(&name, &temporary_value) {
        return Ok(());
      }
    }

    let next_count = self
      .header_list
      .len()
      .checked_add(1)
      .ok_or(WebFetchError::LimitExceeded {
        kind: WebFetchLimitKind::HeaderCount,
        limit: self.limits.max_header_count,
        attempted: usize::MAX,
      })?;
    if next_count > self.limits.max_header_count {
      return Err(WebFetchError::LimitExceeded {
        kind: WebFetchLimitKind::HeaderCount,
        limit: self.limits.max_header_count,
        attempted: next_count,
      });
    }

    let current_total = self.total_header_bytes();
    let entry_bytes = name
      .as_str()
      .len()
      .checked_add(value.len())
      .ok_or(WebFetchError::LimitExceeded {
        kind: WebFetchLimitKind::TotalHeaderBytes,
        limit: self.limits.max_total_header_bytes,
        attempted: usize::MAX,
      })?;
    let next_total = current_total
      .checked_add(entry_bytes)
      .ok_or(WebFetchError::LimitExceeded {
        kind: WebFetchLimitKind::TotalHeaderBytes,
        limit: self.limits.max_total_header_bytes,
        attempted: usize::MAX,
      })?;
    if next_total > self.limits.max_total_header_bytes {
      return Err(WebFetchError::LimitExceeded {
        kind: WebFetchLimitKind::TotalHeaderBytes,
        limit: self.limits.max_total_header_bytes,
        attempted: next_total,
      });
    }

    self
      .header_list
      .push(Header { name, value: value.to_string() });

    if self.guard == HeadersGuard::RequestNoCors {
      self.remove_privileged_no_cors_request_headers();
    }

    Ok(())
  }

  pub fn delete(&mut self, name: &str) -> Result<()> {
    let name = validate_header_name(name)?;
    let dummy_value = "";

    if !self.validate_mutation(&name, dummy_value)? {
      return Ok(());
    }

    if self.guard == HeadersGuard::RequestNoCors
      && !is_no_cors_safelisted_request_header_name(&name)
      && !is_privileged_no_cors_request_header_name(&name)
    {
      return Ok(());
    }

    if !self.header_list_contains(&name) {
      return Ok(());
    }

    self.header_list_delete(&name);

    if self.guard == HeadersGuard::RequestNoCors {
      self.remove_privileged_no_cors_request_headers();
    }

    Ok(())
  }

  pub fn get(&self, name: &str) -> Result<Option<String>> {
    let name = validate_header_name(name)?;
    Ok(self.header_list_get(&name))
  }

  pub fn get_set_cookie(&self) -> Vec<String> {
    let set_cookie = HeaderName::from_static("set-cookie");
    self
      .header_list
      .iter()
      .filter(|header| header.name == set_cookie)
      .map(|header| header.value.clone())
      .collect()
  }

  pub fn has(&self, name: &str) -> Result<bool> {
    let name = validate_header_name(name)?;
    Ok(self.header_list_contains(&name))
  }

  pub fn set(&mut self, name: &str, value: &str) -> Result<()> {
    let name = validate_header_name(name)?;
    let value = normalize_header_value(value);
    validate_header_value(value)?;

    if !self.validate_mutation(&name, value)? {
      return Ok(());
    }

    if self.guard == HeadersGuard::RequestNoCors && !is_no_cors_safelisted_request_header(&name, value) {
      return Ok(());
    }

    let current_total = self.total_header_bytes();

    let name_len = name.as_str().len();
    let next_value_len = value.len();

    let mut first_index: Option<usize> = None;
    let mut to_remove: Vec<usize> = Vec::new();
    for (idx, header) in self.header_list.iter().enumerate() {
      if header.name == name {
        if first_index.is_none() {
          first_index = Some(idx);
        } else {
          to_remove.push(idx);
        }
      }
    }

    let (next_count, next_total) = match first_index {
      Some(first) => {
        let mut removed_bytes: usize = 0;
        for idx in &to_remove {
          if let Some(header) = self.header_list.get(*idx) {
            let entry_bytes = name_len
              .checked_add(header.value.len())
              .ok_or(WebFetchError::LimitExceeded {
                kind: WebFetchLimitKind::TotalHeaderBytes,
                limit: self.limits.max_total_header_bytes,
                attempted: usize::MAX,
              })?;
            removed_bytes = removed_bytes
              .checked_add(entry_bytes)
              .ok_or(WebFetchError::LimitExceeded {
                kind: WebFetchLimitKind::TotalHeaderBytes,
                limit: self.limits.max_total_header_bytes,
                attempted: usize::MAX,
              })?;
          }
        }

        let old_first_value_len = self
          .header_list
          .get(first)
          .map(|h| h.value.len())
          .unwrap_or(0);

        let base = current_total
          .checked_sub(removed_bytes)
          .and_then(|v| v.checked_sub(old_first_value_len))
          .ok_or(WebFetchError::LimitExceeded {
            kind: WebFetchLimitKind::TotalHeaderBytes,
            limit: self.limits.max_total_header_bytes,
            attempted: usize::MAX,
          })?;

        let next_total = base
          .checked_add(next_value_len)
          .ok_or(WebFetchError::LimitExceeded {
            kind: WebFetchLimitKind::TotalHeaderBytes,
            limit: self.limits.max_total_header_bytes,
            attempted: usize::MAX,
          })?;
        let next_count = self
          .header_list
          .len()
          .checked_sub(to_remove.len())
          .ok_or(WebFetchError::LimitExceeded {
            kind: WebFetchLimitKind::HeaderCount,
            limit: self.limits.max_header_count,
            attempted: usize::MAX,
          })?;
        (next_count, next_total)
      }
      None => {
        let next_count = self
          .header_list
          .len()
          .checked_add(1)
          .ok_or(WebFetchError::LimitExceeded {
            kind: WebFetchLimitKind::HeaderCount,
            limit: self.limits.max_header_count,
            attempted: usize::MAX,
          })?;
        let entry_bytes = name_len
          .checked_add(next_value_len)
          .ok_or(WebFetchError::LimitExceeded {
            kind: WebFetchLimitKind::TotalHeaderBytes,
            limit: self.limits.max_total_header_bytes,
            attempted: usize::MAX,
          })?;
        let next_total = current_total
          .checked_add(entry_bytes)
          .ok_or(WebFetchError::LimitExceeded {
            kind: WebFetchLimitKind::TotalHeaderBytes,
            limit: self.limits.max_total_header_bytes,
            attempted: usize::MAX,
          })?;
        (next_count, next_total)
      }
    };

    if next_count > self.limits.max_header_count {
      return Err(WebFetchError::LimitExceeded {
        kind: WebFetchLimitKind::HeaderCount,
        limit: self.limits.max_header_count,
        attempted: next_count,
      });
    }
    if next_total > self.limits.max_total_header_bytes {
      return Err(WebFetchError::LimitExceeded {
        kind: WebFetchLimitKind::TotalHeaderBytes,
        limit: self.limits.max_total_header_bytes,
        attempted: next_total,
      });
    }

    self.header_list_set(name, value.to_string());

    if self.guard == HeadersGuard::RequestNoCors {
      self.remove_privileged_no_cors_request_headers();
    }

    Ok(())
  }

  /// Fill this `Headers` object from an iterator of `(name, value)` pairs.
  ///
  /// This matches the Fetch `Headers` constructor behavior for the `HeadersInit` record and
  /// sequence-of-pairs forms: each pair is appended using [`Headers::append`], so names/values are
  /// normalized and the current [`HeadersGuard`] is enforced.
  pub fn fill_from_pairs<I, N, V>(&mut self, pairs: I) -> Result<()>
  where
    I: IntoIterator<Item = (N, V)>,
    N: AsRef<str>,
    V: AsRef<str>,
  {
    for (name, value) in pairs {
      self.append(name.as_ref(), value.as_ref())?;
    }
    Ok(())
  }

  /// Fill this `Headers` object from the WebIDL `sequence<sequence<ByteString>>` form.
  ///
  /// Each inner sequence must contain exactly two items (`[name, value]`). If an entry has a
  /// different length, this returns [`WebFetchError::HeadersInitSequenceItemWrongLength`] (distinct
  /// from header name/value validation).
  pub fn fill_from_sequence<I, E, S>(&mut self, sequence: I) -> Result<()>
  where
    I: IntoIterator<Item = E>,
    E: AsRef<[S]>,
    S: AsRef<str>,
  {
    for entry in sequence {
      let entry = entry.as_ref();
      if entry.len() != 2 {
        return Err(WebFetchError::HeadersInitSequenceItemWrongLength { len: entry.len() });
      }
      self.append(entry[0].as_ref(), entry[1].as_ref())?;
    }
    Ok(())
  }

  /// Append a raw list of `(name, value)` pairs.
  ///
  /// This is intended for bridging from [`crate::resource::FetchedResource::response_headers`]:
  /// duplicates are preserved and each pair is appended via [`Headers::append`] so guard
  /// enforcement still applies (e.g. `Response` headers ignore `Set-Cookie`).
  pub fn extend_from_raw_pairs(&mut self, pairs: &[(String, String)]) -> Result<()> {
    for (name, value) in pairs {
      self.append(name, value)?;
    }
    Ok(())
  }

  /// Return the underlying header list as raw `(name, value)` pairs.
  ///
  /// - Preserves insertion order and duplicates (unlike [`Headers::sort_and_combine`]).
  /// - Names are returned in their normalized (lowercased) form.
  ///
  /// This is primarily intended for bridging from Fetch core types to the renderer's HTTP
  /// networking stack (`ResourceFetcher::fetch_http_request`).
  pub fn raw_pairs(&self) -> Vec<(String, String)> {
    self
      .header_list
      .iter()
      .map(|header| (header.name.as_str().to_string(), header.value.clone()))
      .collect()
  }

  /// Return this header list using Fetch's "header list sort and combine" algorithm.
  ///
  /// The returned list is suitable for deterministic iteration (`for..of`, `entries()`, etc.):
  ///
  /// - Header names are lowercased and sorted.
  /// - Duplicate names are combined using `", "` (matching [`Headers::get`]).
  /// - `set-cookie` is treated specially: each value is returned as a distinct pair in original
  ///   order.
  pub fn sort_and_combine(&self) -> Vec<(String, String)> {
    // https://fetch.spec.whatwg.org/#concept-header-list-sort-and-combine
    let mut headers: Vec<&Header> = self.header_list.iter().collect();
    headers.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));

    let mut output: Vec<(String, String)> = Vec::new();
    for header in headers {
      let name = header.name.as_str();
      if name == "set-cookie" {
        output.push((name.to_string(), header.value.clone()));
        continue;
      }

      if let Some((_, value)) = output.last_mut().filter(|(out_name, _)| out_name == name) {
        value.push_str(", ");
        value.push_str(&header.value);
      } else {
        output.push((name.to_string(), header.value.clone()));
      }
    }
    output
  }

  fn validate_mutation(&self, name: &HeaderName, value: &str) -> Result<bool> {
    // Fetch "Headers/validate" algorithm:
    // https://fetch.spec.whatwg.org/#concept-headers-validate
    if self.guard == HeadersGuard::Immutable {
      return Err(WebFetchError::HeadersImmutable);
    }

    // `name` is already validated by `validate_header_name` and `value` by `validate_header_value`.
    // Keep the checks in debug builds for caller mistakes when using internal helpers.
    debug_assert!(validate_header_name(name.as_str()).is_ok());
    debug_assert!(validate_header_value(value).is_ok());

    if self.guard == HeadersGuard::Request && is_forbidden_request_header(name, value) {
      return Ok(false);
    }

    if self.guard == HeadersGuard::Response && is_forbidden_response_header_name(name) {
      return Ok(false);
    }

    Ok(true)
  }

  fn header_list_contains(&self, name: &HeaderName) -> bool {
    self.header_list.iter().any(|header| header.name == *name)
  }

  fn header_list_get(&self, name: &HeaderName) -> Option<String> {
    if !self.header_list_contains(name) {
      return None;
    }

    let values: Vec<&str> = self
      .header_list
      .iter()
      .filter(|header| header.name == *name)
      .map(|header| header.value.as_str())
      .collect();

    Some(values.join(", "))
  }

  fn header_list_delete(&mut self, name: &HeaderName) {
    self.header_list.retain(|header| header.name != *name);
  }

  fn header_list_set(&mut self, name: HeaderName, value: String) {
    let mut first_index: Option<usize> = None;
    let mut to_remove: Vec<usize> = Vec::new();

    for (idx, header) in self.header_list.iter().enumerate() {
      if header.name == name {
        if first_index.is_none() {
          first_index = Some(idx);
        } else {
          to_remove.push(idx);
        }
      }
    }

    if let Some(first) = first_index {
      if let Some(header) = self.header_list.get_mut(first) {
        header.value = value;
      }
      // Remove remaining matches from the end so indices stay valid.
      for idx in to_remove.into_iter().rev() {
        self.header_list.remove(idx);
      }
    } else {
      self.header_list.push(Header { name, value });
    }
  }

  fn remove_privileged_no_cors_request_headers(&mut self) {
    // https://fetch.spec.whatwg.org/#concept-headers-remove-privileged-no-cors-request-headers
    let range = HeaderName::from_static("range");
    self.header_list_delete(&range);
  }

  fn total_header_bytes(&self) -> usize {
    let mut total = 0usize;
    for header in &self.header_list {
      total = total.saturating_add(header.name.as_str().len());
      total = total.saturating_add(header.value.len());
    }
    total
  }
}

fn validate_header_name(name: &str) -> Result<HeaderName> {
  HeaderName::from_bytes(name.as_bytes()).map_err(|_| WebFetchError::InvalidHeaderName {
    name: name.to_string(),
  })
}

fn normalize_header_value(value: &str) -> &str {
  // https://fetch.spec.whatwg.org/#concept-header-value-normalize
  value
    .trim_matches(|c| c == ' ' || c == '\t')
}

fn trim_http_whitespace(value: &str) -> &str {
  value.trim_matches(|c: char| matches!(c, ' ' | '\t'))
}

fn validate_header_value(value: &str) -> Result<()> {
  // https://fetch.spec.whatwg.org/#concept-header-value
  if value
    .as_bytes()
    .iter()
    .any(|&b| b == 0x00 || b == b'\r' || b == b'\n')
  {
    return Err(WebFetchError::InvalidHeaderValue {
      value: value.to_string(),
    });
  }

  if value.starts_with([' ', '\t']) || value.ends_with([' ', '\t']) {
    return Err(WebFetchError::InvalidHeaderValue {
      value: value.to_string(),
    });
  }

  Ok(())
}

fn is_forbidden_response_header_name(name: &HeaderName) -> bool {
  matches!(name.as_str(), "set-cookie" | "set-cookie2")
}

fn is_forbidden_request_header(name: &HeaderName, value: &str) -> bool {
  // https://fetch.spec.whatwg.org/#forbidden-header-name
  match name.as_str() {
    "accept-charset"
    | "accept-encoding"
    | "access-control-request-headers"
    | "access-control-request-method"
    | "connection"
    | "content-length"
    | "cookie"
    | "cookie2"
    | "date"
    | "dnt"
    | "expect"
    | "host"
    | "keep-alive"
    | "origin"
    | "referer"
    | "set-cookie"
    | "te"
    | "trailer"
    | "transfer-encoding"
    | "upgrade"
    | "via" => true,
    name_str => {
      if name_str.starts_with("proxy-") || name_str.starts_with("sec-") {
        return true;
      }

      if matches!(
        name_str,
        "x-http-method" | "x-http-method-override" | "x-method-override"
      ) {
        for method in value.split(',').map(trim_http_whitespace) {
          if is_forbidden_method(method) {
            return true;
          }
        }
      }

      false
    }
  }
}

fn is_forbidden_method(method: &str) -> bool {
  matches!(method.to_ascii_uppercase().as_str(), "CONNECT" | "TRACE" | "TRACK")
}

fn is_no_cors_safelisted_request_header_name(name: &HeaderName) -> bool {
  matches!(
    name.as_str(),
    "accept" | "accept-language" | "content-language" | "content-type"
  )
}

fn is_privileged_no_cors_request_header_name(name: &HeaderName) -> bool {
  // https://fetch.spec.whatwg.org/#privileged-no-cors-request-header-name
  name.as_str() == "range"
}

fn is_no_cors_safelisted_request_header(name: &HeaderName, value: &str) -> bool {
  // https://fetch.spec.whatwg.org/#no-cors-safelisted-request-header
  if !is_no_cors_safelisted_request_header_name(name) {
    return false;
  }
  is_cors_safelisted_request_header(name, value)
}

fn is_cors_safelisted_request_header(name: &HeaderName, value: &str) -> bool {
  // https://fetch.spec.whatwg.org/#cors-safelisted-request-header
  if value.as_bytes().len() > 128 {
    return false;
  }

  match name.as_str() {
    "accept" => !value.as_bytes().iter().copied().any(is_cors_unsafe_request_header_byte),
    "accept-language" | "content-language" => value
      .as_bytes()
      .iter()
      .copied()
      .all(is_cors_safelisted_language_byte),
    "content-type" => {
      if value
        .as_bytes()
        .iter()
        .copied()
        .any(is_cors_unsafe_request_header_byte)
      {
        return false;
      }

      let essence = trim_http_whitespace(value.split(';').next().unwrap_or("")).to_ascii_lowercase();

      matches!(
        essence.as_str(),
        "application/x-www-form-urlencoded" | "multipart/form-data" | "text/plain"
      )
    }
    "range" => is_safelisted_range_header_value(value),
    _ => false,
  }
}

fn is_cors_unsafe_request_header_byte(byte: u8) -> bool {
  // https://fetch.spec.whatwg.org/#cors-unsafe-request-header-byte
  if byte < 0x20 && byte != 0x09 {
    return true;
  }
  matches!(
    byte,
    0x22 | 0x28 | 0x29 | 0x3A | 0x3C | 0x3E | 0x3F | 0x40 | 0x5B | 0x5C | 0x5D | 0x7B | 0x7D
      | 0x7F
  )
}

fn is_cors_safelisted_language_byte(byte: u8) -> bool {
  // https://fetch.spec.whatwg.org/#cors-safelisted-request-header
  matches!(byte, b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z' | b' ' | b'*' | b',' | b'-' | b'.' | b';' | b'=')
}

fn is_safelisted_range_header_value(value: &str) -> bool {
  // https://fetch.spec.whatwg.org/#cors-safelisted-request-header (range case)
  let trimmed = trim_http_whitespace(value);
  let Some(rest) = trimmed
    .strip_prefix("bytes=")
    .or_else(|| trimmed.strip_prefix("Bytes="))
    .or_else(|| trimmed.strip_prefix("BYTES="))
  else {
    return false;
  };

  // Only allow `bytes=<start>-<end>` or `bytes=<start>-`.
  let Some((start, end)) = rest.split_once('-') else {
    return false;
  };

  if start.is_empty() {
    return false;
  }

  if !start.chars().all(|c| c.is_ascii_digit()) {
    return false;
  }

  if !end.is_empty() && !end.chars().all(|c| c.is_ascii_digit()) {
    return false;
  }

  true
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn cors_safelisted_range_does_not_trim_non_ascii_whitespace() {
    let nbsp = "\u{00A0}";
    let value = format!("{nbsp}bytes=0-1");
    assert!(!is_safelisted_range_header_value(&value));
    assert!(is_safelisted_range_header_value("bytes=0-1"));
  }

  #[test]
  fn cors_safelisted_content_type_does_not_trim_non_ascii_whitespace() {
    let name = HeaderName::from_static("content-type");
    assert!(is_cors_safelisted_request_header(&name, "text/plain"));
    let nbsp = "\u{00A0}";
    let value = format!("{nbsp}text/plain");
    assert!(!is_cors_safelisted_request_header(&name, &value));
  }
}
