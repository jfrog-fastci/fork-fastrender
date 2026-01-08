use super::{Result, WebFetchError};
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
    }
  }

  pub fn new_with_guard(guard: HeadersGuard) -> Self {
    Self {
      header_list: Vec::new(),
      guard,
    }
  }

  pub fn guard(&self) -> HeadersGuard {
    self.guard
  }

  pub fn set_guard(&mut self, guard: HeadersGuard) {
    self.guard = guard;
  }

  pub fn append(&mut self, name: &str, value: &str) -> Result<()> {
    let name = validate_header_name(name)?;
    let value = normalize_header_value(value);
    validate_header_value(&value)?;

    if !self.validate_mutation(&name, &value)? {
      return Ok(());
    }

    if self.guard == HeadersGuard::RequestNoCors {
      let mut temporary_value = self.header_list_get(&name).unwrap_or_default();
      if temporary_value.is_empty() {
        temporary_value = value.clone();
      } else {
        temporary_value.push_str(", ");
        temporary_value.push_str(&value);
      }

      if !is_no_cors_safelisted_request_header(&name, &temporary_value) {
        return Ok(());
      }
    }

    self.header_list.push(Header { name, value });

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
    validate_header_value(&value)?;

    if !self.validate_mutation(&name, &value)? {
      return Ok(());
    }

    if self.guard == HeadersGuard::RequestNoCors && !is_no_cors_safelisted_request_header(&name, &value) {
      return Ok(());
    }

    self.header_list_set(name, value);

    if self.guard == HeadersGuard::RequestNoCors {
      self.remove_privileged_no_cors_request_headers();
    }

    Ok(())
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
}

fn validate_header_name(name: &str) -> Result<HeaderName> {
  HeaderName::from_bytes(name.as_bytes()).map_err(|_| WebFetchError::InvalidHeaderName {
    name: name.to_string(),
  })
}

fn normalize_header_value(value: &str) -> String {
  // https://fetch.spec.whatwg.org/#concept-header-value-normalize
  value
    .trim_matches(|c| c == ' ' || c == '\t')
    .to_string()
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
        for method in value.split(',').map(|s| s.trim()) {
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

      let essence = value
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();

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
  let trimmed = value.trim();
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

