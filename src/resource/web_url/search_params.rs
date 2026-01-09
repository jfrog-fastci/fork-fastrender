use crate::resource::web_url::error::{WebUrlError, WebUrlLimitKind};
use crate::resource::web_url::limits::WebUrlLimits;
use crate::resource::web_url::WebUrl;
use std::cmp::Ordering;

#[derive(Debug, Clone)]
enum WebUrlSearchParamsInner {
  Standalone { pairs: Vec<(String, String)> },
  Associated { url: WebUrl },
}

#[derive(Debug, Clone)]
pub struct WebUrlSearchParams {
  inner: WebUrlSearchParamsInner,
}

impl WebUrlSearchParams {
  pub fn new() -> Self {
    Self {
      inner: WebUrlSearchParamsInner::Standalone { pairs: Vec::new() },
    }
  }

  pub(crate) fn associated(url: WebUrl) -> Self {
    Self {
      inner: WebUrlSearchParamsInner::Associated { url },
    }
  }

  pub fn len(&self, limits: &WebUrlLimits) -> Result<usize, WebUrlError> {
    match &self.inner {
      WebUrlSearchParamsInner::Standalone { pairs } => Ok(pairs.len()),
      WebUrlSearchParamsInner::Associated { url } => Ok(url.search_params_snapshot(limits)?.len(limits)?),
    }
  }

  pub fn size(&self, limits: &WebUrlLimits) -> Result<usize, WebUrlError> {
    self.len(limits)
  }

  pub fn is_empty(&self, limits: &WebUrlLimits) -> Result<bool, WebUrlError> {
    Ok(self.len(limits)? == 0)
  }

  pub fn parse(input: &str, limits: &WebUrlLimits) -> Result<Self, WebUrlError> {
    let input = input.strip_prefix('?').unwrap_or(input);

    if input.len() > limits.max_input_bytes {
      return Err(WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::InputBytes,
        limit: limits.max_input_bytes,
        attempted: input.len(),
      });
    }

    if input.is_empty() {
      return Ok(Self::new());
    }

    // Capacity hint: count `&` up to the configured max to avoid repeated growth while still
    // being linear time with small constants.
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

      let next_count = pairs
        .len()
        .checked_add(1)
        .ok_or(WebUrlError::LimitExceeded {
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
      let pair_decoded_len = name_decoded_len
        .checked_add(value_decoded_len)
        .ok_or(WebUrlError::LimitExceeded {
          kind: WebUrlLimitKind::TotalQueryBytes,
          limit: limits.max_total_query_bytes,
          attempted: usize::MAX,
        })?;

      let next_total = total_decoded_bytes
        .checked_add(pair_decoded_len)
        .ok_or(WebUrlError::LimitExceeded {
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

      let name = decode_urlencoded_component(name_part, name_decoded_len)?;
      let value = decode_urlencoded_component(value_part, value_decoded_len)?;

      // No further allocations for updating these counters.
      total_decoded_bytes = next_total;

      pairs.try_reserve(1)?;
      pairs.push((name, value));
    }

    Ok(Self {
      inner: WebUrlSearchParamsInner::Standalone { pairs },
    })
  }

  pub fn replace_all(&mut self, input: &str, limits: &WebUrlLimits) -> Result<(), WebUrlError> {
    let parsed = Self::parse(input, limits)?;
    match &mut self.inner {
      WebUrlSearchParamsInner::Standalone { pairs } => {
        *pairs = parsed.into_pairs();
      }
      WebUrlSearchParamsInner::Associated { url } => {
        url.set_search_params(&parsed, limits)?;
      }
    }
    Ok(())
  }

  pub fn from_pairs<I, K, V>(pairs: I, limits: &WebUrlLimits) -> Result<Self, WebUrlError>
  where
    I: IntoIterator<Item = (K, V)>,
    K: AsRef<str>,
    V: AsRef<str>,
  {
    let mut out = Vec::new();
    let mut total_decoded_bytes: usize = 0;

    for (k, v) in pairs.into_iter() {
      let next_count = out.len().checked_add(1).ok_or(WebUrlError::LimitExceeded {
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

      let k = k.as_ref();
      let v = v.as_ref();

      let pair_len = k.len().checked_add(v.len()).ok_or(WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::TotalQueryBytes,
        limit: limits.max_total_query_bytes,
        attempted: usize::MAX,
      })?;

      let next_total = total_decoded_bytes.checked_add(pair_len).ok_or(WebUrlError::LimitExceeded {
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

      let key = try_clone_str(k)?;
      let value = try_clone_str(v)?;

      total_decoded_bytes = next_total;
      out.try_reserve(1)?;
      out.push((key, value));
    }

    Ok(Self {
      inner: WebUrlSearchParamsInner::Standalone { pairs: out },
    })
  }

  pub fn append(&mut self, name: &str, value: &str, limits: &WebUrlLimits) -> Result<(), WebUrlError> {
    match &mut self.inner {
      WebUrlSearchParamsInner::Standalone { pairs } => append_pair(pairs, name, value, limits),
      WebUrlSearchParamsInner::Associated { url } => {
        let mut params = url.search_params_snapshot(limits)?;
        params.append(name, value, limits)?;
        url.set_search_params(&params, limits)
      }
    }
  }

  pub fn delete(
    &mut self,
    name: &str,
    value: Option<&str>,
    limits: &WebUrlLimits,
  ) -> Result<(), WebUrlError> {
    match &mut self.inner {
      WebUrlSearchParamsInner::Standalone { pairs } => {
        match value {
          Some(value) => pairs.retain(|(n, v)| !(n == name && v == value)),
          None => pairs.retain(|(n, _)| n != name),
        }
        Ok(())
      }
      WebUrlSearchParamsInner::Associated { url } => {
        let mut params = url.search_params_snapshot(limits)?;
        params.delete(name, value, limits)?;
        url.set_search_params(&params, limits)
      }
    }
  }

  pub fn get(&self, name: &str, limits: &WebUrlLimits) -> Result<Option<String>, WebUrlError> {
    match &self.inner {
      WebUrlSearchParamsInner::Standalone { pairs } => pairs
        .iter()
        .find_map(|(n, v)| if n == name { Some(try_clone_str(v)) } else { None })
        .transpose(),
      WebUrlSearchParamsInner::Associated { url } => url.search_params_snapshot(limits)?.get(name, limits),
    }
  }

  pub fn get_all(&self, name: &str, limits: &WebUrlLimits) -> Result<Vec<String>, WebUrlError> {
    match &self.inner {
      WebUrlSearchParamsInner::Standalone { pairs } => {
        let mut out = Vec::new();
        for (n, v) in pairs.iter() {
          if n == name {
            out.try_reserve(1)?;
            out.push(try_clone_str(v)?);
          }
        }
        Ok(out)
      }
      WebUrlSearchParamsInner::Associated { url } => url.search_params_snapshot(limits)?.get_all(name, limits),
    }
  }

  pub fn has(&self, name: &str, value: Option<&str>, limits: &WebUrlLimits) -> Result<bool, WebUrlError> {
    match &self.inner {
      WebUrlSearchParamsInner::Standalone { pairs } => Ok(match value {
        Some(value) => pairs.iter().any(|(n, v)| n == name && v == value),
        None => pairs.iter().any(|(n, _)| n == name),
      }),
      WebUrlSearchParamsInner::Associated { url } => url.search_params_snapshot(limits)?.has(name, value, limits),
    }
  }

  pub fn set(&mut self, name: &str, value: &str, limits: &WebUrlLimits) -> Result<(), WebUrlError> {
    match &mut self.inner {
      WebUrlSearchParamsInner::Standalone { pairs } => set_pair(pairs, name, value, limits),
      WebUrlSearchParamsInner::Associated { url } => {
        let mut params = url.search_params_snapshot(limits)?;
        params.set(name, value, limits)?;
        url.set_search_params(&params, limits)
      }
    }
  }

  pub fn sort(&mut self, limits: &WebUrlLimits) -> Result<(), WebUrlError> {
    match &mut self.inner {
      WebUrlSearchParamsInner::Standalone { pairs } => {
        sort_pairs_by_name_code_units(pairs);
        Ok(())
      }
      WebUrlSearchParamsInner::Associated { url } => {
        let mut params = url.search_params_snapshot(limits)?;
        params.sort(limits)?;
        url.set_search_params(&params, limits)
      }
    }
  }

  pub fn serialize(&self, limits: &WebUrlLimits) -> Result<String, WebUrlError> {
    match &self.inner {
      WebUrlSearchParamsInner::Standalone { pairs } => serialize_pairs(pairs, limits),
      WebUrlSearchParamsInner::Associated { url } => url.search_params_snapshot(limits)?.serialize(limits),
    }
  }

  fn into_pairs(self) -> Vec<(String, String)> {
    match self.inner {
      WebUrlSearchParamsInner::Standalone { pairs } => pairs,
      WebUrlSearchParamsInner::Associated { .. } => Vec::new(),
    }
  }
}

fn serialize_pairs(pairs: &[(String, String)], limits: &WebUrlLimits) -> Result<String, WebUrlError> {
  if pairs.is_empty() {
    return Ok(String::new());
  }

  let mut bytes = Vec::new();
  let mut written: usize = 0;
  for (idx, (name, value)) in pairs.iter().enumerate() {
    if idx != 0 {
      push_byte_limited(&mut bytes, b'&', &mut written, limits.max_input_bytes)?;
    }
    append_urlencoded_limited(
      &mut bytes,
      name.as_bytes(),
      &mut written,
      limits.max_input_bytes,
    )?;
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

fn append_pair(
  pairs: &mut Vec<(String, String)>,
  name: &str,
  value: &str,
  limits: &WebUrlLimits,
) -> Result<(), WebUrlError> {
  let next_count = pairs
    .len()
    .checked_add(1)
    .ok_or(WebUrlError::LimitExceeded {
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

  let mut total_decoded_bytes: usize = 0;
  for (n, v) in pairs.iter() {
    let pair_len = n
      .len()
      .checked_add(v.len())
      .ok_or(WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::TotalQueryBytes,
        limit: limits.max_total_query_bytes,
        attempted: usize::MAX,
      })?;
    total_decoded_bytes = total_decoded_bytes
      .checked_add(pair_len)
      .ok_or(WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::TotalQueryBytes,
        limit: limits.max_total_query_bytes,
        attempted: usize::MAX,
      })?;
  }

  let add_len = name
    .len()
    .checked_add(value.len())
    .ok_or(WebUrlError::LimitExceeded {
      kind: WebUrlLimitKind::TotalQueryBytes,
      limit: limits.max_total_query_bytes,
      attempted: usize::MAX,
    })?;
  let next_total = total_decoded_bytes.checked_add(add_len).ok_or(WebUrlError::LimitExceeded {
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

  let name = try_clone_str(name)?;
  let value = try_clone_str(value)?;

  pairs.try_reserve(1)?;
  pairs.push((name, value));
  Ok(())
}

fn set_pair(
  pairs: &mut Vec<(String, String)>,
  name: &str,
  value: &str,
  limits: &WebUrlLimits,
) -> Result<(), WebUrlError> {
  let mut first_idx: Option<usize> = None;
  let mut new_total_bytes: usize = 0;
  let mut new_len: usize = 0;

  for (idx, (n, v)) in pairs.iter().enumerate() {
    if n == name {
      if first_idx.is_none() {
        first_idx = Some(idx);
        new_len = new_len.checked_add(1).ok_or(WebUrlError::LimitExceeded {
          kind: WebUrlLimitKind::QueryPairs,
          limit: limits.max_query_pairs,
          attempted: usize::MAX,
        })?;

        let pair_len = n
          .len()
          .checked_add(value.len())
          .ok_or(WebUrlError::LimitExceeded {
            kind: WebUrlLimitKind::TotalQueryBytes,
            limit: limits.max_total_query_bytes,
            attempted: usize::MAX,
          })?;
        new_total_bytes = new_total_bytes.checked_add(pair_len).ok_or(WebUrlError::LimitExceeded {
          kind: WebUrlLimitKind::TotalQueryBytes,
          limit: limits.max_total_query_bytes,
          attempted: usize::MAX,
        })?;
      }
      // Remove duplicates (skip).
      continue;
    }

    new_len = new_len.checked_add(1).ok_or(WebUrlError::LimitExceeded {
      kind: WebUrlLimitKind::QueryPairs,
      limit: limits.max_query_pairs,
      attempted: usize::MAX,
    })?;

    let pair_len = n
      .len()
      .checked_add(v.len())
      .ok_or(WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::TotalQueryBytes,
        limit: limits.max_total_query_bytes,
        attempted: usize::MAX,
      })?;
    new_total_bytes = new_total_bytes.checked_add(pair_len).ok_or(WebUrlError::LimitExceeded {
      kind: WebUrlLimitKind::TotalQueryBytes,
      limit: limits.max_total_query_bytes,
      attempted: usize::MAX,
    })?;
  }

  if first_idx.is_none() {
    new_len = new_len.checked_add(1).ok_or(WebUrlError::LimitExceeded {
      kind: WebUrlLimitKind::QueryPairs,
      limit: limits.max_query_pairs,
      attempted: usize::MAX,
    })?;

    let pair_len = name
      .len()
      .checked_add(value.len())
      .ok_or(WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::TotalQueryBytes,
        limit: limits.max_total_query_bytes,
        attempted: usize::MAX,
      })?;
    new_total_bytes = new_total_bytes.checked_add(pair_len).ok_or(WebUrlError::LimitExceeded {
      kind: WebUrlLimitKind::TotalQueryBytes,
      limit: limits.max_total_query_bytes,
      attempted: usize::MAX,
    })?;
  }

  if new_len > limits.max_query_pairs {
    return Err(WebUrlError::LimitExceeded {
      kind: WebUrlLimitKind::QueryPairs,
      limit: limits.max_query_pairs,
      attempted: new_len,
    });
  }
  if new_total_bytes > limits.max_total_query_bytes {
    return Err(WebUrlError::LimitExceeded {
      kind: WebUrlLimitKind::TotalQueryBytes,
      limit: limits.max_total_query_bytes,
      attempted: new_total_bytes,
    });
  }

  match first_idx {
    Some(idx) => {
      let replacement = try_clone_str(value)?;
      pairs[idx].1 = replacement;

      let mut seen = false;
      pairs.retain(|(n, _)| {
        if n == name {
          if seen {
            false
          } else {
            seen = true;
            true
          }
        } else {
          true
        }
      });
      Ok(())
    }
    None => {
      let name = try_clone_str(name)?;
      let value = try_clone_str(value)?;
      pairs.try_reserve(1)?;
      pairs.push((name, value));
      Ok(())
    }
  }
}

fn sort_pairs_by_name_code_units(pairs: &mut Vec<(String, String)>) {
  for i in 1..pairs.len() {
    let mut j = i;
    while j > 0
      && cmp_by_code_unit_order(&pairs[j - 1].0, &pairs[j].0) == Ordering::Greater
    {
      pairs.swap(j - 1, j);
      j -= 1;
    }
  }
}

fn cmp_by_code_unit_order(a: &str, b: &str) -> Ordering {
  let mut a = a.encode_utf16();
  let mut b = b.encode_utf16();
  loop {
    match (a.next(), b.next()) {
      (Some(a), Some(b)) => match a.cmp(&b) {
        Ordering::Equal => continue,
        other => return other,
      },
      (None, Some(_)) => return Ordering::Less,
      (Some(_), None) => return Ordering::Greater,
      (None, None) => return Ordering::Equal,
    }
  }
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

fn decode_urlencoded_component(input: &str, decoded_len: usize) -> Result<String, WebUrlError> {
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

  // Enforce `decoded_len` strictly: if our accounting ever diverges, clamp to a safe error.
  if out.len() != decoded_len {
    return Err(WebUrlError::InvalidUtf8);
  }

  String::from_utf8(out).map_err(|_| WebUrlError::InvalidUtf8)
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
      b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
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

#[cfg(test)]
mod tests {
  use super::*;
  use crate::resource::web_url::{WebUrlError, WebUrlLimitKind, WebUrlLimits};

  #[test]
  fn web_url_search_params_delete_with_value_removes_only_matching_pairs() {
    let limits = WebUrlLimits::default();
    let mut params = WebUrlSearchParams::parse("a=1&a=2&a=1&b=0", &limits).unwrap();
    params.delete("a", Some("1"), &limits).unwrap();
    assert_eq!(params.serialize(&limits).unwrap(), "a=2&b=0");
  }

  #[test]
  fn web_url_search_params_has_with_value() {
    let limits = WebUrlLimits::default();
    let params = WebUrlSearchParams::parse("a=1&a=2", &limits).unwrap();
    assert!(params.has("a", Some("2"), &limits).unwrap());
    assert!(!params.has("a", Some("3"), &limits).unwrap());
    assert!(!params.has("b", Some("x"), &limits).unwrap());
  }

  #[test]
  fn web_url_search_params_sort_is_stable() {
    let limits = WebUrlLimits::default();
    // WHATWG URLSearchParams example: sorts by name and preserves relative ordering among equal names.
    let mut params = WebUrlSearchParams::parse("z=b&a=b&z=a&a=a", &limits).unwrap();
    params.sort(&limits).unwrap();
    assert_eq!(params.serialize(&limits).unwrap(), "a=b&a=a&z=b&z=a");
  }

  #[test]
  fn web_url_search_params_is_live_and_updates_url_query_on_mutation() {
    let limits = WebUrlLimits::default();
    let url = WebUrl::parse("https://example.com/?a=1&b=2", &limits).unwrap();
    let mut params = url.search_params();

    params.append("c", "3", &limits).unwrap();
    assert_eq!(url.href(&limits).unwrap(), "https://example.com/?a=1&b=2&c=3");
  }

  #[test]
  fn web_url_search_params_append_errors_when_exceeding_pair_limit() {
    let limits = WebUrlLimits {
      max_input_bytes: 1024,
      max_query_pairs: 1,
      max_total_query_bytes: 1024,
    };

    let mut params = WebUrlSearchParams::new();
    params.append("a", "1", &limits).unwrap();
    let err = params.append("b", "2", &limits).unwrap_err();
    assert_eq!(
      err,
      WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::QueryPairs,
        limit: 1,
        attempted: 2
      }
    );
  }

  #[test]
  fn web_url_search_params_append_errors_when_exceeding_total_query_bytes() {
    let limits = WebUrlLimits {
      max_input_bytes: 1024,
      max_query_pairs: 1024,
      max_total_query_bytes: 3,
    };

    let mut params = WebUrlSearchParams::new();
    let err = params.append("a", "bcd", &limits).unwrap_err();
    assert_eq!(
      err,
      WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::TotalQueryBytes,
        limit: 3,
        attempted: 4
      }
    );
  }
}
