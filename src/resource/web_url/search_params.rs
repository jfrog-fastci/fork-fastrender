use std::cell::RefCell;
use std::rc::Rc;

use crate::resource::web_url::error::{WebUrlError, WebUrlLimitKind};
use crate::resource::web_url::limits::WebUrlLimits;
use crate::resource::web_url::url::WebUrlInner;

#[derive(Clone, Debug)]
enum WebUrlSearchParamsInner {
  /// A standalone `URLSearchParams` list with its own storage.
  Standalone {
    pairs: Rc<RefCell<Vec<(String, String)>>>,
  },
  /// A `URLSearchParams` view over an associated `WebUrl`.
  Associated {
    url: Rc<RefCell<WebUrlInner>>,
  },
}

/// A bounded, WHATWG-shaped URLSearchParams list that preserves duplicates and stable ordering.
#[derive(Clone, Debug)]
pub struct WebUrlSearchParams {
  inner: WebUrlSearchParamsInner,
  limits: WebUrlLimits,
}

impl WebUrlSearchParams {
  pub fn new(limits: &WebUrlLimits) -> Self {
    Self {
      inner: WebUrlSearchParamsInner::Standalone {
        pairs: Rc::new(RefCell::new(Vec::new())),
      },
      limits: limits.clone(),
    }
  }

  /// Parse a raw query string such as `"a=b&c=d"` or a leading-`?` variant such as `"?a=b"`.
  pub fn parse(input: &str, limits: &WebUrlLimits) -> Result<Self, WebUrlError> {
    let pairs = parse_urlencoded_pairs(input, limits)?;
    Ok(Self {
      inner: WebUrlSearchParamsInner::Standalone {
        pairs: Rc::new(RefCell::new(pairs)),
      },
      limits: limits.clone(),
    })
  }

  pub(crate) fn associated(url: Rc<RefCell<WebUrlInner>>, limits: WebUrlLimits) -> Self {
    Self {
      inner: WebUrlSearchParamsInner::Associated { url },
      limits,
    }
  }

  pub fn len(&self) -> Result<usize, WebUrlError> {
    match &self.inner {
      WebUrlSearchParamsInner::Standalone { pairs } => Ok(pairs.borrow().len()),
      WebUrlSearchParamsInner::Associated { url } => {
        let url = url.borrow();
        let pairs = parse_urlencoded_pairs(url.url.query().unwrap_or(""), &self.limits)?;
        Ok(pairs.len())
      }
    }
  }

  /// Equivalent to the WHATWG `URLSearchParams.size` getter.
  pub fn size(&self) -> Result<usize, WebUrlError> {
    self.len()
  }

  pub fn is_empty(&self) -> Result<bool, WebUrlError> {
    Ok(self.len()? == 0)
  }

  /// Return a cloned list of name/value pairs in stable list order.
  pub fn pairs(&self) -> Result<Vec<(String, String)>, WebUrlError> {
    match &self.inner {
      WebUrlSearchParamsInner::Standalone { pairs } => {
        let pairs = pairs.borrow();
        let mut out = Vec::new();
        out.try_reserve(pairs.len())?;
        for (n, v) in pairs.iter() {
          out.try_reserve(1)?;
          out.push((try_clone_str(n)?, try_clone_str(v)?));
        }
        Ok(out)
      }
      WebUrlSearchParamsInner::Associated { url } => {
        let url = url.borrow();
        parse_urlencoded_pairs(url.url.query().unwrap_or(""), &self.limits)
      }
    }
  }

  pub fn get(&self, name: &str) -> Result<Option<String>, WebUrlError> {
    match &self.inner {
      WebUrlSearchParamsInner::Standalone { pairs } => Ok(pairs
        .borrow()
        .iter()
        .find_map(|(n, v)| if n == name { Some(v.as_str()) } else { None })
        .map(try_clone_str)
        .transpose()?),
      WebUrlSearchParamsInner::Associated { url } => {
        let url = url.borrow();
        let pairs = parse_urlencoded_pairs(url.url.query().unwrap_or(""), &self.limits)?;
        for (n, v) in pairs {
          if n == name {
            return Ok(Some(v));
          }
        }
        Ok(None)
      }
    }
  }

  pub fn get_all(&self, name: &str) -> Result<Vec<String>, WebUrlError> {
    match &self.inner {
      WebUrlSearchParamsInner::Standalone { pairs } => {
        let pairs = pairs.borrow();
        let mut out = Vec::new();
        for (n, v) in pairs.iter() {
          if n == name {
            out.try_reserve(1)?;
            out.push(try_clone_str(v)?);
          }
        }
        Ok(out)
      }
      WebUrlSearchParamsInner::Associated { url } => {
        let url = url.borrow();
        let pairs = parse_urlencoded_pairs(url.url.query().unwrap_or(""), &self.limits)?;
        let mut out = Vec::new();
        for (n, v) in pairs {
          if n == name {
            out.try_reserve(1)?;
            out.push(v);
          }
        }
        Ok(out)
      }
    }
  }

  pub fn has(&self, name: &str, value: Option<&str>) -> Result<bool, WebUrlError> {
    match &self.inner {
      WebUrlSearchParamsInner::Standalone { pairs } => Ok(match value {
        None => pairs.borrow().iter().any(|(n, _)| n == name),
        Some(value) => pairs.borrow().iter().any(|(n, v)| n == name && v == value),
      }),
      WebUrlSearchParamsInner::Associated { url } => {
        let url = url.borrow();
        let pairs = parse_urlencoded_pairs(url.url.query().unwrap_or(""), &self.limits)?;
        Ok(match value {
          None => pairs.into_iter().any(|(n, _)| n == name),
          Some(value) => pairs.into_iter().any(|(n, v)| n == name && v == value),
        })
      }
    }
  }

  pub fn append(&self, name: &str, value: &str) -> Result<(), WebUrlError> {
    match &self.inner {
      WebUrlSearchParamsInner::Standalone { pairs } => {
        let mut pairs = pairs.borrow_mut();
        enforce_append_limits(&pairs, name, value, &self.limits)?;

        let name = try_clone_str(name)?;
        let value = try_clone_str(value)?;

        pairs.try_reserve(1)?;
        pairs.push((name, value));

        // Ensure the resulting list remains serializable within output limits.
        if let Err(err) = serialize_urlencoded_pairs(&pairs, &self.limits) {
          pairs.pop();
          return Err(err);
        }
        Ok(())
      }
      WebUrlSearchParamsInner::Associated { url } => {
        self.mutate_associated(url, |pairs| {
          enforce_append_limits(pairs, name, value, &self.limits)?;
          pairs.try_reserve(1)?;
          pairs.push((try_clone_str(name)?, try_clone_str(value)?));
          Ok(())
        })
      }
    }
  }

  pub fn delete(&self, name: &str, value: Option<&str>) -> Result<(), WebUrlError> {
    match &self.inner {
      WebUrlSearchParamsInner::Standalone { pairs } => {
        match value {
          None => pairs.borrow_mut().retain(|(n, _)| n != name),
          Some(value) => pairs.borrow_mut().retain(|(n, v)| n != name || v != value),
        }
        Ok(())
      }
      WebUrlSearchParamsInner::Associated { url } => self.mutate_associated(url, |pairs| {
        match value {
          None => pairs.retain(|(n, _)| n != name),
          Some(value) => pairs.retain(|(n, v)| n != name || v != value),
        }
        Ok(())
      }),
    }
  }

  /// Set the first matching pair's value and remove any remaining pairs with the same name.
  ///
  /// If no existing pair matches `name`, append a new pair to the end of the list.
  pub fn set(&self, name: &str, value: &str) -> Result<(), WebUrlError> {
    match &self.inner {
      WebUrlSearchParamsInner::Standalone { pairs } => {
        let pairs_ref = pairs.borrow();
        enforce_set_limits(&pairs_ref, name, value, &self.limits)?;

        let old_len = pairs_ref.len();
        let mut out: Vec<(String, String)> = Vec::new();
        out.try_reserve(old_len.saturating_add(1))?;

        let mut inserted = false;
        let mut new_value = try_clone_str(value)?;
        let mut new_key = if pairs_ref.iter().any(|(n, _)| n == name) {
          None
        } else {
          Some(try_clone_str(name)?)
        };

        for (n, v) in pairs_ref.iter() {
          if n == name {
            if !inserted {
              inserted = true;
              out.push((try_clone_str(n)?, std::mem::take(&mut new_value)));
            }
          } else {
            out.push((try_clone_str(n)?, try_clone_str(v)?));
          }
        }
        if !inserted {
          out.push((new_key.take().expect("new_key set when !inserted"), new_value));
        }

        // Ensure the new list remains serializable within output limits before committing the
        // mutation.
        serialize_urlencoded_pairs(&out, &self.limits)?;

        drop(pairs_ref);
        *pairs.borrow_mut() = out;
        Ok(())
      }
      WebUrlSearchParamsInner::Associated { url } => self.mutate_associated(url, |pairs| {
        enforce_set_limits(pairs, name, value, &self.limits)?;

        let old_len = pairs.len();
        let mut out: Vec<(String, String)> = Vec::new();
        out.try_reserve(old_len.saturating_add(1))?;

        let mut inserted = false;
        let mut new_value = try_clone_str(value)?;
        let mut new_key = if pairs.iter().any(|(n, _)| n == name) {
          None
        } else {
          Some(try_clone_str(name)?)
        };

        let old = std::mem::take(pairs);
        for (n, v) in old.into_iter() {
          if n == name {
            if !inserted {
              inserted = true;
              out.push((n, std::mem::take(&mut new_value)));
            }
          } else {
            out.push((n, v));
          }
        }
        if !inserted {
          out.push((new_key.take().expect("new_key set when !inserted"), new_value));
        }

        *pairs = out;
        Ok(())
      }),
    }
  }

  pub fn sort(&self) -> Result<(), WebUrlError> {
    match &self.inner {
      WebUrlSearchParamsInner::Standalone { pairs } => {
        pairs.borrow_mut().sort_by(|(a, _), (b, _)| cmp_utf16(a, b));
        Ok(())
      }
      WebUrlSearchParamsInner::Associated { url } => self.mutate_associated(url, |pairs| {
        pairs.sort_by(|(a, _), (b, _)| cmp_utf16(a, b));
        Ok(())
      }),
    }
  }

  /// Serialize this list using the x-www-form-urlencoded rules.
  pub fn serialize(&self) -> Result<String, WebUrlError> {
    match &self.inner {
      WebUrlSearchParamsInner::Standalone { pairs } => {
        let pairs = pairs.borrow();
        serialize_urlencoded_pairs(&pairs, &self.limits)
      }
      WebUrlSearchParamsInner::Associated { url } => {
        let url = url.borrow();
        let pairs = parse_urlencoded_pairs(url.url.query().unwrap_or(""), &self.limits)?;
        serialize_urlencoded_pairs(&pairs, &self.limits)
      }
    }
  }

  fn mutate_associated<F>(&self, url: &Rc<RefCell<WebUrlInner>>, f: F) -> Result<(), WebUrlError>
  where
    F: FnOnce(&mut Vec<(String, String)>) -> Result<(), WebUrlError>,
  {
    let mut inner = url.borrow_mut();
    let before = inner.url.clone();

    let query = inner.url.query().unwrap_or("");
    let mut pairs = parse_urlencoded_pairs(query, &self.limits)?;

    f(&mut pairs)?;

    let serialized = serialize_urlencoded_pairs(&pairs, &self.limits)?;
    if serialized.is_empty() {
      inner.url.set_query(None);
    } else {
      inner.url.set_query(Some(serialized.as_str()));
    }

    let attempted = inner.url.as_str().len();
    if attempted > self.limits.max_input_bytes {
      inner.url = before;
      return Err(WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::InputBytes,
        limit: self.limits.max_input_bytes,
        attempted,
      });
    }

    Ok(())
  }
}

fn enforce_append_limits(
  pairs: &[(String, String)],
  name: &str,
  value: &str,
  limits: &WebUrlLimits,
) -> Result<(), WebUrlError> {
  let next_count = pairs.len().checked_add(1).ok_or(WebUrlError::LimitExceeded {
    kind: WebUrlLimitKind::QueryPairs,
    limit: limits.max_query_pairs,
    attempted: usize::MAX,
  })?;
  if next_count > limits.max_query_pairs {
    return Err(WebUrlError::LimitExceeded {
      kind: WebUrlLimitKind::QueryPairs,
      limit: limits.max_query_pairs,
      attempted: next_count,
    });
  }

  let current_total = total_decoded_bytes(pairs, limits)?;
  let pair_len = name.len().checked_add(value.len()).ok_or(WebUrlError::LimitExceeded {
    kind: WebUrlLimitKind::TotalQueryBytes,
    limit: limits.max_total_query_bytes,
    attempted: usize::MAX,
  })?;
  let next_total = current_total.checked_add(pair_len).ok_or(WebUrlError::LimitExceeded {
    kind: WebUrlLimitKind::TotalQueryBytes,
    limit: limits.max_total_query_bytes,
    attempted: usize::MAX,
  })?;
  if next_total > limits.max_total_query_bytes {
    return Err(WebUrlError::LimitExceeded {
      kind: WebUrlLimitKind::TotalQueryBytes,
      limit: limits.max_total_query_bytes,
      attempted: next_total,
    });
  }

  Ok(())
}

fn enforce_set_limits(
  pairs: &[(String, String)],
  name: &str,
  value: &str,
  limits: &WebUrlLimits,
) -> Result<(), WebUrlError> {
  let mut next_count: usize = 0;
  let mut next_total: usize = 0;
  let mut inserted = false;

  for (n, v) in pairs {
    if n == name {
      if inserted {
        continue;
      }
      inserted = true;
      next_count = next_count.checked_add(1).ok_or(WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::QueryPairs,
        limit: limits.max_query_pairs,
        attempted: usize::MAX,
      })?;
      let pair_len = n.len().checked_add(value.len()).ok_or(WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::TotalQueryBytes,
        limit: limits.max_total_query_bytes,
        attempted: usize::MAX,
      })?;
      next_total = next_total.checked_add(pair_len).ok_or(WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::TotalQueryBytes,
        limit: limits.max_total_query_bytes,
        attempted: usize::MAX,
      })?;
    } else {
      next_count = next_count.checked_add(1).ok_or(WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::QueryPairs,
        limit: limits.max_query_pairs,
        attempted: usize::MAX,
      })?;
      let pair_len = n.len().checked_add(v.len()).ok_or(WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::TotalQueryBytes,
        limit: limits.max_total_query_bytes,
        attempted: usize::MAX,
      })?;
      next_total = next_total.checked_add(pair_len).ok_or(WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::TotalQueryBytes,
        limit: limits.max_total_query_bytes,
        attempted: usize::MAX,
      })?;
    }
  }

  if !inserted {
    next_count = next_count.checked_add(1).ok_or(WebUrlError::LimitExceeded {
      kind: WebUrlLimitKind::QueryPairs,
      limit: limits.max_query_pairs,
      attempted: usize::MAX,
    })?;
    let pair_len = name.len().checked_add(value.len()).ok_or(WebUrlError::LimitExceeded {
      kind: WebUrlLimitKind::TotalQueryBytes,
      limit: limits.max_total_query_bytes,
      attempted: usize::MAX,
    })?;
    next_total = next_total.checked_add(pair_len).ok_or(WebUrlError::LimitExceeded {
      kind: WebUrlLimitKind::TotalQueryBytes,
      limit: limits.max_total_query_bytes,
      attempted: usize::MAX,
    })?;
  }

  if next_count > limits.max_query_pairs {
    return Err(WebUrlError::LimitExceeded {
      kind: WebUrlLimitKind::QueryPairs,
      limit: limits.max_query_pairs,
      attempted: next_count,
    });
  }
  if next_total > limits.max_total_query_bytes {
    return Err(WebUrlError::LimitExceeded {
      kind: WebUrlLimitKind::TotalQueryBytes,
      limit: limits.max_total_query_bytes,
      attempted: next_total,
    });
  }

  Ok(())
}

fn total_decoded_bytes(pairs: &[(String, String)], limits: &WebUrlLimits) -> Result<usize, WebUrlError> {
  let mut total: usize = 0;
  for (n, v) in pairs {
    let pair_len = n.len().checked_add(v.len()).ok_or(WebUrlError::LimitExceeded {
      kind: WebUrlLimitKind::TotalQueryBytes,
      limit: limits.max_total_query_bytes,
      attempted: usize::MAX,
    })?;
    total = total.checked_add(pair_len).ok_or(WebUrlError::LimitExceeded {
      kind: WebUrlLimitKind::TotalQueryBytes,
      limit: limits.max_total_query_bytes,
      attempted: usize::MAX,
    })?;
  }
  Ok(total)
}

fn parse_urlencoded_pairs(input: &str, limits: &WebUrlLimits) -> Result<Vec<(String, String)>, WebUrlError> {
  let input = input.strip_prefix('?').unwrap_or(input);

  if input.len() > limits.max_input_bytes {
    return Err(WebUrlError::LimitExceeded {
      kind: WebUrlLimitKind::InputBytes,
      limit: limits.max_input_bytes,
      attempted: input.len(),
    });
  }

  if input.is_empty() {
    return Ok(Vec::new());
  }

  // Capacity hint: count `&` up to the configured max to avoid repeated growth while still being
  // linear time with small constants.
  let mut estimate = 1usize;
  for byte in input.as_bytes() {
    if *byte == b'&' {
      estimate = estimate.saturating_add(1);
      if estimate >= limits.max_query_pairs {
        break;
      }
    }
  }
  estimate = estimate.min(limits.max_query_pairs);

  let mut pairs = Vec::new();
  pairs.try_reserve(estimate)?;

  let mut total_decoded_bytes: usize = 0;

  for part in input.split('&') {
    if part.is_empty() {
      continue;
    }

    let next_count = pairs.len().checked_add(1).ok_or(WebUrlError::LimitExceeded {
      kind: WebUrlLimitKind::QueryPairs,
      limit: limits.max_query_pairs,
      attempted: usize::MAX,
    })?;
    if next_count > limits.max_query_pairs {
      return Err(WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::QueryPairs,
        limit: limits.max_query_pairs,
        attempted: next_count,
      });
    }

    let (name_part, value_part) = match part.split_once('=') {
      Some((name, value)) => (name, value),
      None => (part, ""),
    };

    let name_decoded_len = urlencoded_decoded_len(name_part);
    let value_decoded_len = urlencoded_decoded_len(value_part);
    let (name, name_bytes) = decode_urlencoded_component_limited(
      name_part,
      name_decoded_len,
      total_decoded_bytes,
      limits.max_total_query_bytes,
    )?;
    let total_after_name = total_decoded_bytes.checked_add(name_bytes).ok_or(WebUrlError::LimitExceeded {
      kind: WebUrlLimitKind::TotalQueryBytes,
      limit: limits.max_total_query_bytes,
      attempted: usize::MAX,
    })?;
    let (value, value_bytes) = decode_urlencoded_component_limited(
      value_part,
      value_decoded_len,
      total_after_name,
      limits.max_total_query_bytes,
    )?;

    let next_total = total_after_name.checked_add(value_bytes).ok_or(WebUrlError::LimitExceeded {
      kind: WebUrlLimitKind::TotalQueryBytes,
      limit: limits.max_total_query_bytes,
      attempted: usize::MAX,
    })?;
    if next_total > limits.max_total_query_bytes {
      return Err(WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::TotalQueryBytes,
        limit: limits.max_total_query_bytes,
        attempted: next_total,
      });
    }

    total_decoded_bytes = next_total;
    pairs.try_reserve(1)?;
    pairs.push((name, value));
  }

  Ok(pairs)
}

fn serialize_urlencoded_pairs(
  pairs: &[(String, String)],
  limits: &WebUrlLimits,
) -> Result<String, WebUrlError> {
  if pairs.is_empty() {
    return Ok(String::new());
  }

  let mut bytes = Vec::new();
  let mut written: usize = 0;
  for (idx, (name, value)) in pairs.iter().enumerate() {
    if idx != 0 {
      push_byte_limited(&mut bytes, b'&', &mut written, limits.max_input_bytes)?;
    }
    append_urlencoded_limited(&mut bytes, name.as_bytes(), &mut written, limits.max_input_bytes)?;
    push_byte_limited(&mut bytes, b'=', &mut written, limits.max_input_bytes)?;
    append_urlencoded_limited(
      &mut bytes,
      value.as_bytes(),
      &mut written,
      limits.max_input_bytes,
    )?;
  }

  // The output is ASCII; UTF-8 conversion should never fail.
  String::from_utf8(bytes).map_err(|_| WebUrlError::InvalidUtf8)
}

fn try_clone_str(value: &str) -> Result<String, WebUrlError> {
  let mut out = String::new();
  out.try_reserve_exact(value.len())?;
  out.push_str(value);
  Ok(out)
}

fn hex_value(byte: u8) -> Option<u8> {
  match byte {
    b'0'..=b'9' => Some(byte - b'0'),
    b'a'..=b'f' => Some(byte - b'a' + 10),
    b'A'..=b'F' => Some(byte - b'A' + 10),
    _ => None,
  }
}

fn urlencoded_decoded_len(input: &str) -> usize {
  let bytes = input.as_bytes();
  let mut i = 0usize;
  let mut out_len = 0usize;
  while i < bytes.len() {
    if bytes[i] == b'%' && i + 2 < bytes.len() {
      if hex_value(bytes[i + 1]).is_some() && hex_value(bytes[i + 2]).is_some() {
        out_len = out_len.saturating_add(1);
        i += 3;
        continue;
      }
    }
    out_len = out_len.saturating_add(1);
    i += 1;
  }
  out_len
}

fn decode_urlencoded_component_limited(
  input: &str,
  decoded_len: usize,
  total_so_far: usize,
  max_total: usize,
) -> Result<(String, usize), WebUrlError> {
  let bytes = input.as_bytes();
  let mut out = Vec::new();
  out.try_reserve_exact(decoded_len)?;

  let mut i = 0usize;
  while i < bytes.len() {
    match bytes[i] {
      b'+' => {
        push_byte_checked(&mut out, b' ', decoded_len)?;
        i += 1;
      }
      b'%' if i + 2 < bytes.len() => {
        if let (Some(h1), Some(h2)) = (hex_value(bytes[i + 1]), hex_value(bytes[i + 2])) {
          push_byte_checked(&mut out, (h1 << 4) | h2, decoded_len)?;
          i += 3;
        } else {
          push_byte_checked(&mut out, b'%', decoded_len)?;
          i += 1;
        }
      }
      byte => {
        push_byte_checked(&mut out, byte, decoded_len)?;
        i += 1;
      }
    }
  }

  // If our decoding length accounting ever diverges, clamp to the observed length to keep bounded.
  // This should never happen as `urlencoded_decoded_len` matches the logic above, but keep the
  // parser robust to future edits.
  if out.len() != decoded_len {
    out.truncate(decoded_len.min(out.len()));
  }

  decode_utf8_lossy_limited(&out, total_so_far, max_total)
}

fn push_byte_checked(out: &mut Vec<u8>, byte: u8, max_len: usize) -> Result<(), WebUrlError> {
  let next_len = out.len().checked_add(1).ok_or(WebUrlError::InvalidUtf8)?;
  if next_len > max_len {
    return Err(WebUrlError::InvalidUtf8);
  }
  out.try_reserve(1)?;
  out.push(byte);
  Ok(())
}

fn decode_utf8_lossy_limited(
  input: &[u8],
  total_so_far: usize,
  max_total: usize,
) -> Result<(String, usize), WebUrlError> {
  let mut out = String::new();
  // Lossy output can grow due to U+FFFD (3 bytes). Reserve only what the remaining budget allows.
  let reserve_hint = input.len().min(max_total.saturating_sub(total_so_far));
  out.try_reserve(reserve_hint)?;

  let mut written: usize = 0;
  let mut i: usize = 0;
  while i < input.len() {
    match std::str::from_utf8(&input[i..]) {
      Ok(valid) => {
        push_str_limited(&mut out, valid, &mut written, total_so_far, max_total)?;
        break;
      }
      Err(err) => {
        let valid_up_to = err.valid_up_to();
        if valid_up_to > 0 {
          // SAFETY: `valid_up_to` is guaranteed to be on a UTF-8 boundary by `Utf8Error`.
          let valid = unsafe { std::str::from_utf8_unchecked(&input[i..i + valid_up_to]) };
          push_str_limited(&mut out, valid, &mut written, total_so_far, max_total)?;
        }

        push_str_limited(&mut out, "\u{FFFD}", &mut written, total_so_far, max_total)?;

        let advance = err.error_len().unwrap_or(1);
        i = i.saturating_add(valid_up_to).saturating_add(advance);
      }
    }
  }

  Ok((out, written))
}

fn push_str_limited(
  out: &mut String,
  s: &str,
  written: &mut usize,
  total_so_far: usize,
  max_total: usize,
) -> Result<(), WebUrlError> {
  let next_written = written.checked_add(s.len()).ok_or(WebUrlError::LimitExceeded {
    kind: WebUrlLimitKind::TotalQueryBytes,
    limit: max_total,
    attempted: usize::MAX,
  })?;
  let next_total = total_so_far.checked_add(next_written).ok_or(WebUrlError::LimitExceeded {
    kind: WebUrlLimitKind::TotalQueryBytes,
    limit: max_total,
    attempted: usize::MAX,
  })?;
  if next_total > max_total {
    return Err(WebUrlError::LimitExceeded {
      kind: WebUrlLimitKind::TotalQueryBytes,
      limit: max_total,
      attempted: next_total,
    });
  }

  out.try_reserve(s.len())?;
  out.push_str(s);
  *written = next_written;
  Ok(())
}

fn push_byte_limited(
  output: &mut Vec<u8>,
  byte: u8,
  written: &mut usize,
  max_bytes: usize,
) -> Result<(), WebUrlError> {
  let next = written.checked_add(1).ok_or(WebUrlError::LimitExceeded {
    kind: WebUrlLimitKind::InputBytes,
    limit: max_bytes,
    attempted: usize::MAX,
  })?;
  if next > max_bytes {
    return Err(WebUrlError::LimitExceeded {
      kind: WebUrlLimitKind::InputBytes,
      limit: max_bytes,
      attempted: next,
    });
  }
  output.try_reserve(1)?;
  output.push(byte);
  *written = next;
  Ok(())
}

fn append_urlencoded_limited(
  output: &mut Vec<u8>,
  input: &[u8],
  written: &mut usize,
  max_bytes: usize,
) -> Result<(), WebUrlError> {
  for &byte in input {
    match byte {
      b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'*' | b'-' | b'.' | b'_' => {
        push_byte_limited(output, byte, written, max_bytes)?;
      }
      b' ' => {
        push_byte_limited(output, b'+', written, max_bytes)?;
      }
      other => {
        // `%XX`
        let next = written.checked_add(3).ok_or(WebUrlError::LimitExceeded {
          kind: WebUrlLimitKind::InputBytes,
          limit: max_bytes,
          attempted: usize::MAX,
        })?;
        if next > max_bytes {
          return Err(WebUrlError::LimitExceeded {
            kind: WebUrlLimitKind::InputBytes,
            limit: max_bytes,
            attempted: next,
          });
        }
        output.try_reserve(3)?;
        output.push(b'%');
        output.push(hex_upper(other >> 4));
        output.push(hex_upper(other & 0x0f));
        *written = next;
      }
    }
  }
  Ok(())
}

fn hex_upper(value: u8) -> u8 {
  match value {
    0..=9 => b'0' + value,
    10..=15 => b'A' + (value - 10),
    _ => b'0',
  }
}

fn cmp_utf16(a: &str, b: &str) -> std::cmp::Ordering {
  let mut a_it = a.encode_utf16();
  let mut b_it = b.encode_utf16();
  loop {
    match (a_it.next(), b_it.next()) {
      (None, None) => return std::cmp::Ordering::Equal,
      (None, Some(_)) => return std::cmp::Ordering::Less,
      (Some(_), None) => return std::cmp::Ordering::Greater,
      (Some(x), Some(y)) => {
        if x != y {
          return x.cmp(&y);
        }
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use super::WebUrlSearchParams;
  use crate::resource::web_url::{WebUrlError, WebUrlLimitKind, WebUrlLimits};

  #[test]
  fn serializes_tilde_using_form_encode_set() {
    let limits = WebUrlLimits::default();
    let params = WebUrlSearchParams::parse("a=b ~", &limits).unwrap();
    assert_eq!(params.serialize().unwrap(), "a=b+%7E");
  }

  #[test]
  fn serializes_asterisk_without_encoding() {
    let limits = WebUrlLimits::default();
    let params = WebUrlSearchParams::parse("a=b*", &limits).unwrap();
    assert_eq!(params.serialize().unwrap(), "a=b*");
  }

  #[test]
  fn roundtrips_plus_and_percent2b() {
    let limits = WebUrlLimits::default();
    let params = WebUrlSearchParams::parse("a+b=c%2B+d", &limits).unwrap();
    assert_eq!(
      params.pairs().unwrap(),
      vec![("a b".to_string(), "c+ d".to_string())]
    );
    assert_eq!(params.serialize().unwrap(), "a+b=c%2B+d");
  }

  #[test]
  fn serialize_limit_counts_percent_encoding_expansion() {
    // "~" is not in the WHATWG application/x-www-form-urlencoded safe set, so serializing expands
    // it to "%7E". Ensure we correctly enforce `max_input_bytes` after that expansion.
    let limits = WebUrlLimits {
      max_input_bytes: 3, // "a=~" fits, but "a=%7E" does not.
      max_query_pairs: 8,
      max_total_query_bytes: 64,
    };
    let params = WebUrlSearchParams::parse("a=~", &limits).unwrap();
    let err = params.serialize().unwrap_err();
    assert_eq!(
      err,
      WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::InputBytes,
        limit: 3,
        attempted: 5,
      }
    );
  }

  #[test]
  fn parse_limit_rejects_too_many_pairs() {
    let limits = WebUrlLimits {
      max_query_pairs: 2,
      ..WebUrlLimits::default()
    };
    let err = WebUrlSearchParams::parse("a=1&b=2&c=3", &limits).unwrap_err();
    assert_eq!(
      err,
      WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::QueryPairs,
        limit: 2,
        attempted: 3,
      }
    );
  }

  #[test]
  fn parse_limit_rejects_too_many_total_query_bytes() {
    let limits = WebUrlLimits {
      max_total_query_bytes: 5,
      ..WebUrlLimits::default()
    };
    let err = WebUrlSearchParams::parse("aaaaa=b", &limits).unwrap_err();
    assert_eq!(
      err,
      WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::TotalQueryBytes,
        limit: 5,
        attempted: 6,
      }
    );
  }

  #[test]
  fn parse_decodes_invalid_utf8_lossily() {
    let limits = WebUrlLimits::default();
    let params = WebUrlSearchParams::parse("a=%FF%FF", &limits).unwrap();
    assert_eq!(params.get("a").unwrap(), Some("\u{FFFD}\u{FFFD}".to_string()));
  }

  #[test]
  fn parse_counts_lossy_replacement_bytes_toward_limit() {
    // "%FF" percent-decodes to a single 0xFF byte, which is not valid UTF-8. URLSearchParams uses
    // UTF-8 decode with replacement, so this becomes U+FFFD (3 bytes). Ensure our decoded bytes
    // accounting enforces the post-decoding string length, not the raw percent-decoded length.
    let limits = WebUrlLimits {
      max_total_query_bytes: 2,
      ..WebUrlLimits::default()
    };
    let err = WebUrlSearchParams::parse("a=%FF", &limits).unwrap_err();
    assert_eq!(
      err,
      WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::TotalQueryBytes,
        limit: 2,
        attempted: 4,
      }
    );
  }
}
