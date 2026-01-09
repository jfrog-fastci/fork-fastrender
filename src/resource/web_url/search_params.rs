use crate::resource::web_url::error::{WebUrlError, WebUrlLimitKind};
use crate::resource::web_url::limits::WebUrlLimits;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebUrlSearchParams {
  pairs: Vec<(String, String)>,
}

impl WebUrlSearchParams {
  pub fn new() -> Self {
    Self { pairs: Vec::new() }
  }

  pub fn len(&self) -> usize {
    self.pairs.len()
  }

  pub fn is_empty(&self) -> bool {
    self.pairs.is_empty()
  }

  pub fn pairs(&self) -> &[(String, String)] {
    &self.pairs
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

    Ok(Self { pairs })
  }

  pub fn replace_all(&mut self, input: &str, limits: &WebUrlLimits) -> Result<(), WebUrlError> {
    let parsed = Self::parse(input, limits)?;
    self.pairs = parsed.pairs;
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

    Ok(Self { pairs: out })
  }

  pub fn serialize(&self, limits: &WebUrlLimits) -> Result<String, WebUrlError> {
    if self.pairs.is_empty() {
      return Ok(String::new());
    }

    let mut bytes = Vec::new();
    let mut written: usize = 0;
    for (idx, (name, value)) in self.pairs.iter().enumerate() {
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

  pub fn append(
    &mut self,
    name: &str,
    value: &str,
    limits: &WebUrlLimits,
  ) -> Result<(), WebUrlError> {
    let next_count = self
      .pairs
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

    let (current_total, next_total) = total_query_bytes_checked(&self.pairs, name, value, limits)?;
    if next_total > limits.max_total_query_bytes {
      return Err(WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::TotalQueryBytes,
        limit: limits.max_total_query_bytes,
        attempted: next_total,
      });
    }

    self.pairs.try_reserve(1)?;
    self.pairs.push((try_clone_str(name)?, try_clone_str(value)?));

    // Ensure the resulting list can be serialized under the configured output limits.
    if let Err(err) = self.serialize(limits) {
      self.pairs.pop();
      // Restore byte accounting (no allocations required).
      debug_assert_eq!(total_query_bytes(&self.pairs), current_total);
      return Err(err);
    }

    Ok(())
  }

  pub fn delete(&mut self, name: &str, value: Option<&str>) {
    match value {
      None => self.pairs.retain(|(n, _)| n != name),
      Some(value) => self.pairs.retain(|(n, v)| n != name || v != value),
    }
  }

  pub fn get(&self, name: &str) -> Option<&str> {
    self
      .pairs
      .iter()
      .find_map(|(n, v)| if n == name { Some(v.as_str()) } else { None })
  }

  pub fn get_all(&self, name: &str) -> Vec<&str> {
    self
      .pairs
      .iter()
      .filter_map(|(n, v)| if n == name { Some(v.as_str()) } else { None })
      .collect()
  }

  pub fn has(&self, name: &str, value: Option<&str>) -> bool {
    match value {
      None => self.pairs.iter().any(|(n, _)| n == name),
      Some(value) => self.pairs.iter().any(|(n, v)| n == name && v == value),
    }
  }

  pub fn set(&mut self, name: &str, value: &str, limits: &WebUrlLimits) -> Result<(), WebUrlError> {
    // Build the new list without mutating `self` until we've validated all limits.
    let mut out: Vec<(String, String)> = Vec::new();
    out.try_reserve(self.pairs.len().saturating_add(1))?;

    let mut seen = false;
    for (n, v) in self.pairs.iter() {
      if n == name {
        if !seen {
          seen = true;
          out.push((try_clone_str(n)?, try_clone_str(value)?));
        }
      } else {
        out.push((try_clone_str(n)?, try_clone_str(v)?));
      }
    }
    if !seen {
      out.push((try_clone_str(name)?, try_clone_str(value)?));
    }

    if out.len() > limits.max_query_pairs {
      return Err(WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::QueryPairs,
        limit: limits.max_query_pairs,
        attempted: out.len(),
      });
    }

    let total = total_query_bytes(&out);
    if total > limits.max_total_query_bytes {
      return Err(WebUrlError::LimitExceeded {
        kind: WebUrlLimitKind::TotalQueryBytes,
        limit: limits.max_total_query_bytes,
        attempted: total,
      });
    }

    let candidate = WebUrlSearchParams { pairs: out };
    candidate.serialize(limits)?;
    let WebUrlSearchParams { pairs } = candidate;
    self.pairs = pairs;
    Ok(())
  }

  pub fn sort(&mut self) {
    self.pairs.sort_by(|(a, _), (b, _)| cmp_utf16(a, b));
  }
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
      // The application/x-www-form-urlencoded percent-encode set leaves:
      //   ALPHA / DIGIT / "*" / "-" / "." / "_"
      // unescaped and percent-encodes everything else, with space mapped to `+`.
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

fn total_query_bytes(pairs: &[(String, String)]) -> usize {
  pairs
    .iter()
    .map(|(n, v)| n.len().saturating_add(v.len()))
    .sum()
}

fn total_query_bytes_checked(
  pairs: &[(String, String)],
  name: &str,
  value: &str,
  limits: &WebUrlLimits,
) -> Result<(usize, usize), WebUrlError> {
  let current_total = total_query_bytes(pairs);
  let added = name.len().checked_add(value.len()).ok_or(WebUrlError::LimitExceeded {
    kind: WebUrlLimitKind::TotalQueryBytes,
    limit: limits.max_total_query_bytes,
    attempted: usize::MAX,
  })?;
  let next_total = current_total.checked_add(added).ok_or(WebUrlError::LimitExceeded {
    kind: WebUrlLimitKind::TotalQueryBytes,
    limit: limits.max_total_query_bytes,
    attempted: usize::MAX,
  })?;
  Ok((current_total, next_total))
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
