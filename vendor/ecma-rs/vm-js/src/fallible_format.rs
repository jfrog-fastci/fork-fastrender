use crate::VmError;

/// Maximum UTF-8 byte length for dynamically constructed error strings.
///
/// Many runtime error paths need to include attacker-controlled strings (identifier names, property
/// keys, import attribute keys, etc). Those must be bounded to avoid building arbitrarily large
/// host (Rust) heap strings.
pub(crate) const MAX_ERROR_MESSAGE_BYTES: usize = 4096;

const TRUNCATION_MARKER: &str = "...";

#[inline]
fn truncate_to_char_boundary(s: &str, max_bytes: usize) -> &str {
  if s.len() <= max_bytes {
    return s;
  }
  let mut end = max_bytes;
  while end > 0 && !s.is_char_boundary(end) {
    end -= 1;
  }
  &s[..end]
}

/// Fallible `push_str` for building host-owned error strings.
#[inline]
pub(crate) fn try_push_str(out: &mut String, s: &str) -> Result<(), VmError> {
  out.try_reserve(s.len()).map_err(|_| VmError::OutOfMemory)?;
  out.push_str(s);
  Ok(())
}

/// Fallible `push(char)` for building host-owned error strings.
#[inline]
pub(crate) fn try_push_char(out: &mut String, ch: char) -> Result<(), VmError> {
  let mut buf = [0u8; 4];
  let encoded = ch.encode_utf8(&mut buf);
  out
    .try_reserve(encoded.len())
    .map_err(|_| VmError::OutOfMemory)?;
  out.push(ch);
  Ok(())
}

/// Writes `value` as decimal digits without allocating an intermediate `String`.
pub(crate) fn try_write_u32(out: &mut String, mut value: u32) -> Result<(), VmError> {
  // u32::MAX has 10 digits.
  let mut buf = [0u8; 10];
  let mut i = buf.len();
  if value == 0 {
    i -= 1;
    buf[i] = b'0';
  } else {
    while value != 0 {
      let digit = (value % 10) as u8;
      value /= 10;
      i -= 1;
      buf[i] = b'0' + digit;
    }
  }
  let s = std::str::from_utf8(&buf[i..]).map_err(|_| {
    VmError::InvariantViolation("invalid UTF-8 in u32 decimal digit formatting buffer")
  })?;
  try_push_str(out, s)
}

/// Formats a single dynamic insertion surrounded by static prefix/suffix.
///
/// This utility:
/// - bounds the output length to [`MAX_ERROR_MESSAGE_BYTES`],
/// - uses fallible host allocations (`try_reserve_exact`), and
/// - appends [`TRUNCATION_MARKER`] if truncation occurs and there is space.
pub(crate) fn try_format_error_message(
  prefix: &str,
  insertion: &str,
  suffix: &str,
) -> Result<String, VmError> {
  // If the static parts already exceed the cap, drop the dynamic insertion and truncate the rest.
  if prefix.len().saturating_add(suffix.len()) >= MAX_ERROR_MESSAGE_BYTES {
    let mut out = String::new();
    out
      .try_reserve_exact(MAX_ERROR_MESSAGE_BYTES)
      .map_err(|_| VmError::OutOfMemory)?;

    let prefix_part = truncate_to_char_boundary(prefix, MAX_ERROR_MESSAGE_BYTES);
    try_push_str(&mut out, prefix_part)?;
    if out.len() < MAX_ERROR_MESSAGE_BYTES {
      let remaining = MAX_ERROR_MESSAGE_BYTES - out.len();
      let suffix_part = truncate_to_char_boundary(suffix, remaining);
      try_push_str(&mut out, suffix_part)?;
    }
    return Ok(out);
  }

  let available = MAX_ERROR_MESSAGE_BYTES - prefix.len() - suffix.len();
  let (insertion_part, truncated) = if insertion.len() <= available {
    (insertion, false)
  } else if available >= TRUNCATION_MARKER.len() {
    (
      truncate_to_char_boundary(insertion, available - TRUNCATION_MARKER.len()),
      true,
    )
  } else {
    // Not enough space even for the truncation marker; include only a prefix of the insertion.
    (truncate_to_char_boundary(insertion, available), false)
  };

  let final_len = prefix.len()
    + insertion_part.len()
    + suffix.len()
    + if truncated {
      TRUNCATION_MARKER.len()
    } else {
      0
    };

  let mut out = String::new();
  out
    .try_reserve_exact(final_len)
    .map_err(|_| VmError::OutOfMemory)?;

  try_push_str(&mut out, prefix)?;
  try_push_str(&mut out, insertion_part)?;
  if truncated {
    for ch in TRUNCATION_MARKER.chars() {
      try_push_char(&mut out, ch)?;
    }
  }
  try_push_str(&mut out, suffix)?;
  Ok(out)
}

/// Convenience wrapper for common "identifier + static prefix" error messages.
#[inline]
pub(crate) fn try_format_identifier_error(prefix: &str, ident: &str) -> Result<String, VmError> {
  try_format_error_message(prefix, ident, "")
}
