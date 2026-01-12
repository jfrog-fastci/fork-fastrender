use crate::tick::{tick_every, DEFAULT_TICK_EVERY};
use crate::fallible_format::try_write_u32;
use crate::{GcString, Heap, Value, VmError};

/// ECMAScript `ToNumber` for the supported value types.
///
/// This is the VM's minimal `ToNumber` used by heap-internal operations and WebIDL-style
/// conversions. Full spec `ToNumber` for objects requires `ToPrimitive`, which can invoke user
/// code; use `Scope::to_number` in evaluator/built-in call sites where a `Vm` + host context
/// exists.
pub fn to_number(heap: &mut Heap, value: Value) -> Result<f64, VmError> {
  to_number_with_tick(heap, value, &mut || Ok(()))
}

/// Implements `ToNumber` for a String value, without requiring a mutable [`Heap`] borrow.
///
/// This is useful for internal algorithms like `CanonicalNumericIndexString` that only need
/// string-to-number parsing and should not force callers to take `&mut Heap`.
pub(crate) fn string_to_number(heap: &Heap, s: GcString) -> Result<f64, VmError> {
  let mut tick = || Ok(());
  string_to_number_with_tick(heap, s, &mut tick)
}

/// Budget-aware variant of [`to_number`].
pub(crate) fn to_number_with_tick(
  heap: &mut Heap,
  value: Value,
  tick: &mut impl FnMut() -> Result<(), VmError>,
) -> Result<f64, VmError> {
  match value {
    Value::Undefined => Ok(f64::NAN),
    Value::Null => Ok(0.0),
    Value::Bool(b) => Ok(if b { 1.0 } else { 0.0 }),
    Value::Number(n) => Ok(n),
    Value::BigInt(_) => Err(VmError::TypeError(
      "Cannot convert a BigInt value to a number",
    )),
    Value::String(s) => string_to_number_with_tick(heap, s, tick),
    Value::Symbol(_) => Err(VmError::TypeError("Cannot convert a Symbol value to a number")),
    Value::Object(_) => Err(VmError::Unimplemented(
      "ToNumber on objects requires ToPrimitive/built-ins",
    )),
  }
}

fn string_to_number_with_tick(
  heap: &Heap,
  s: GcString,
  tick: &mut impl FnMut() -> Result<(), VmError>,
) -> Result<f64, VmError> {
  let units = heap.get_string(s)?.as_code_units();

  // TrimString (ECMA-262): trim ECMAScript WhiteSpace + LineTerminator.
  let mut start = 0usize;
  let mut steps = 0usize;
  while start < units.len() && is_ecma_whitespace_unit(units[start]) {
    tick_every(steps, DEFAULT_TICK_EVERY, tick)?;
    steps = steps.wrapping_add(1);
    start += 1;
  }
  let mut end = units.len();
  while end > start && is_ecma_whitespace_unit(units[end - 1]) {
    tick_every(steps, DEFAULT_TICK_EVERY, tick)?;
    steps = steps.wrapping_add(1);
    end -= 1;
  }

  let trimmed = &units[start..end];
  if trimmed.is_empty() {
    return Ok(0.0);
  }

  // Infinity is case-sensitive in ECMAScript string numeric literals.
  const INFINITY_UNITS: [u16; 8] = [73, 110, 102, 105, 110, 105, 116, 121]; // "Infinity"
  const PLUS_INFINITY_UNITS: [u16; 9] = [43, 73, 110, 102, 105, 110, 105, 116, 121]; // "+Infinity"
  const MINUS_INFINITY_UNITS: [u16; 9] = [45, 73, 110, 102, 105, 110, 105, 116, 121]; // "-Infinity"
  if trimmed == INFINITY_UNITS || trimmed == PLUS_INFINITY_UNITS {
    return Ok(f64::INFINITY);
  }
  if trimmed == MINUS_INFINITY_UNITS {
    return Ok(f64::NEG_INFINITY);
  }

  // Per ECMA-262, signed hex/binary/octal literals are not valid `StringToNumber` inputs.
  // E.g. `Number("-0x10")` is `NaN` (use `parseInt` for signed radix parsing).
  let mut idx = 0usize;
  let mut has_sign = false;
  if trimmed.get(0).copied() == Some(b'+' as u16) || trimmed.get(0).copied() == Some(b'-' as u16) {
    has_sign = true;
    idx = 1;
  }
  if has_sign {
    let rest = &trimmed[idx..];
    if rest.len() >= 2
      && rest[0] == b'0' as u16
      && matches!(
        rest[1],
        u if u == b'x' as u16
          || u == b'X' as u16
          || u == b'b' as u16
          || u == b'B' as u16
          || u == b'o' as u16
          || u == b'O' as u16
      )
    {
      return Ok(f64::NAN);
    }
  }

  // 0x / 0b / 0o literals (unsigned only).
  if trimmed.len() >= 2
    && trimmed[0] == b'0' as u16
    && (trimmed[1] == b'x' as u16 || trimmed[1] == b'X' as u16)
  {
    return Ok(parse_ascii_int_radix_units(&trimmed[2..], 16, tick)?.unwrap_or(f64::NAN));
  }
  if trimmed.len() >= 2
    && trimmed[0] == b'0' as u16
    && (trimmed[1] == b'b' as u16 || trimmed[1] == b'B' as u16)
  {
    return Ok(parse_ascii_int_radix_units(&trimmed[2..], 2, tick)?.unwrap_or(f64::NAN));
  }
  if trimmed.len() >= 2
    && trimmed[0] == b'0' as u16
    && (trimmed[1] == b'o' as u16 || trimmed[1] == b'O' as u16)
  {
    return Ok(parse_ascii_int_radix_units(&trimmed[2..], 8, tick)?.unwrap_or(f64::NAN));
  }

  Ok(parse_ascii_decimal_to_f64_units(trimmed, tick)?.unwrap_or(f64::NAN))
}

fn parse_ascii_int_radix_units(
  units: &[u16],
  radix: u32,
  tick: &mut impl FnMut() -> Result<(), VmError>,
) -> Result<Option<f64>, VmError> {
  if units.is_empty() {
    return Ok(None);
  }
  let radix_f = radix as f64;
  let mut value = 0.0f64;
  for (i, &u) in units.iter().enumerate() {
    tick_every(i, DEFAULT_TICK_EVERY, tick)?;
    if u > 0x7F {
      return Ok(None);
    }
    let b = u as u8;
    let digit: u32 = match b {
      b'0'..=b'9' => (b - b'0') as u32,
      b'a'..=b'f' => (b - b'a' + 10) as u32,
      b'A'..=b'F' => (b - b'A' + 10) as u32,
      _ => return Ok(None),
    };
    if digit >= radix {
      return Ok(None);
    }
    value = value * radix_f + digit as f64;
  }
  Ok(Some(value))
}

pub(crate) fn parse_ascii_decimal_to_f64_units(
  units: &[u16],
  tick: &mut impl FnMut() -> Result<(), VmError>,
) -> Result<Option<f64>, VmError> {
  // ASCII-only `StrDecimalLiteral` with optional sign and exponent.
  //
  // This routine is budget-aware. It performs an O(n) scan over the input while periodically
  // calling `tick()`, and then constructs a bounded-length scientific-notation string for the
  // final float conversion.

  if units.is_empty() {
    return Ok(None);
  }

  // Optional sign.
  let mut i = 0usize;
  let mut sign = 1.0f64;
  if units[i] == b'+' as u16 {
    i += 1;
  } else if units[i] == b'-' as u16 {
    sign = -1.0;
    i += 1;
  }
  if i >= units.len() {
    return Ok(None);
  }

  let mut digits_seen: usize = 0;
  let mut dot_index: Option<usize> = None;
  let mut first_sig_index: Option<usize> = None;

  const MAX_SIG_DIGITS: usize = 1024;
  let mut sig_digits: Vec<u8> = Vec::new();
  sig_digits
    .try_reserve_exact(MAX_SIG_DIGITS.min(units.len()))
    .map_err(|_| VmError::OutOfMemory)?;

  let mut saw_digit = false;

  while i < units.len() {
    let u = units[i];
    if u == b'.' as u16 {
      if dot_index.is_some() {
        return Ok(None);
      }
      dot_index = Some(digits_seen);
      i += 1;
      continue;
    }
    if u == b'e' as u16 || u == b'E' as u16 {
      break;
    }

    if u > 0x7F {
      return Ok(None);
    }
    if !(b'0' as u16..=b'9' as u16).contains(&u) {
      return Ok(None);
    }

    saw_digit = true;

    let digit = (u - b'0' as u16) as u8;
    if first_sig_index.is_none() && digit != 0 {
      first_sig_index = Some(digits_seen);
    }
    if first_sig_index.is_some() && sig_digits.len() < MAX_SIG_DIGITS {
      sig_digits.push(b'0' + digit);
    }

    digits_seen += 1;
    tick_every(digits_seen, DEFAULT_TICK_EVERY, tick)?;
    i += 1;
  }

  if !saw_digit {
    return Ok(None);
  }

  // Exponent part.
  let mut exp_part: i32 = 0;
  if i < units.len() && (units[i] == b'e' as u16 || units[i] == b'E' as u16) {
    i += 1;
    if i >= units.len() {
      return Ok(None);
    }

    let mut exp_sign: i32 = 1;
    if units[i] == b'+' as u16 {
      i += 1;
    } else if units[i] == b'-' as u16 {
      exp_sign = -1;
      i += 1;
    }
    if i >= units.len() {
      return Ok(None);
    }

    let mut saw_exp_digit = false;
    let mut exp: i32 = 0;
    let mut exp_steps: usize = 0;
    while i < units.len() {
      let u = units[i];
      if u > 0x7F {
        return Ok(None);
      }
      if !(b'0' as u16..=b'9' as u16).contains(&u) {
        return Ok(None);
      }
      saw_exp_digit = true;

      let d = (u - b'0' as u16) as i32;
      exp = exp.saturating_mul(10).saturating_add(d);
      exp = exp.min(1_000_000);

      exp_steps += 1;
      tick_every(exp_steps, DEFAULT_TICK_EVERY, tick)?;
      i += 1;
    }
    if !saw_exp_digit {
      return Ok(None);
    }
    exp_part = exp.saturating_mul(exp_sign);
  }

  if i != units.len() {
    return Ok(None);
  }

  // All digits were zero.
  let Some(first_sig_index) = first_sig_index else {
    return Ok(Some(if sign.is_sign_negative() { -0.0 } else { 0.0 }));
  };

  let dot_index = dot_index.unwrap_or(digits_seen);
  let exp10 = (dot_index as i64)
    .saturating_sub(first_sig_index as i64)
    .saturating_sub(1)
    .saturating_add(exp_part as i64);

  debug_assert!(!sig_digits.is_empty());

  // Build a bounded scientific-notation string: "-d.dddde<exp10>".
  let mut s = String::with_capacity(1 + sig_digits.len() + 2 + 24);
  if sign.is_sign_negative() {
    s.push('-');
  }
  s.push(sig_digits[0] as char);
  if sig_digits.len() > 1 {
    s.push('.');
    for &b in &sig_digits[1..] {
      s.push(b as char);
    }
  }
  s.push('e');
  if exp10 < 0 {
    s.push('-');
    try_write_u32(&mut s, (-exp10) as u32)?;
  } else {
    try_write_u32(&mut s, exp10 as u32)?;
  }

  match fast_float::parse::<f64, _>(&s) {
    Ok(n) => Ok(Some(n)),
    Err(_) => Ok(None),
  }
}

pub(crate) fn parse_ascii_decimal_to_f64_str(
  raw: &str,
  tick: &mut impl FnMut() -> Result<(), VmError>,
) -> Result<Option<f64>, VmError> {
  let bytes = raw.as_bytes();
  if bytes.is_empty() {
    return Ok(None);
  }

  // Optional sign.
  let mut i = 0usize;
  let mut sign = 1.0f64;
  if bytes[i] == b'+' {
    i += 1;
  } else if bytes[i] == b'-' {
    sign = -1.0;
    i += 1;
  }
  if i >= bytes.len() {
    return Ok(None);
  }

  let mut digits_seen: usize = 0;
  let mut dot_index: Option<usize> = None;
  let mut first_sig_index: Option<usize> = None;

  const MAX_SIG_DIGITS: usize = 1024;
  let mut sig_digits: Vec<u8> = Vec::new();
  sig_digits
    .try_reserve_exact(MAX_SIG_DIGITS.min(bytes.len()))
    .map_err(|_| VmError::OutOfMemory)?;

  let mut saw_digit = false;

  while i < bytes.len() {
    let b = bytes[i];
    if b == b'.' {
      if dot_index.is_some() {
        return Ok(None);
      }
      dot_index = Some(digits_seen);
      i += 1;
      continue;
    }
    if b == b'e' || b == b'E' {
      break;
    }
    if !b.is_ascii_digit() {
      return Ok(None);
    }

    saw_digit = true;
    let digit = b - b'0';
    if first_sig_index.is_none() && digit != 0 {
      first_sig_index = Some(digits_seen);
    }
    if first_sig_index.is_some() && sig_digits.len() < MAX_SIG_DIGITS {
      sig_digits.push(b'0' + digit);
    }

    digits_seen += 1;
    tick_every(digits_seen, DEFAULT_TICK_EVERY, tick)?;
    i += 1;
  }

  if !saw_digit {
    return Ok(None);
  }

  let mut exp_part: i32 = 0;
  if i < bytes.len() && (bytes[i] == b'e' || bytes[i] == b'E') {
    i += 1;
    if i >= bytes.len() {
      return Ok(None);
    }

    let mut exp_sign: i32 = 1;
    if bytes[i] == b'+' {
      i += 1;
    } else if bytes[i] == b'-' {
      exp_sign = -1;
      i += 1;
    }
    if i >= bytes.len() {
      return Ok(None);
    }

    let mut saw_exp_digit = false;
    let mut exp: i32 = 0;
    let mut exp_steps: usize = 0;
    while i < bytes.len() {
      let b = bytes[i];
      if !b.is_ascii_digit() {
        return Ok(None);
      }
      saw_exp_digit = true;
      exp = exp
        .saturating_mul(10)
        .saturating_add((b - b'0') as i32);
      exp = exp.min(1_000_000);
      exp_steps += 1;
      tick_every(exp_steps, DEFAULT_TICK_EVERY, tick)?;
      i += 1;
    }
    if !saw_exp_digit {
      return Ok(None);
    }
    exp_part = exp.saturating_mul(exp_sign);
  }

  if i != bytes.len() {
    return Ok(None);
  }

  let Some(first_sig_index) = first_sig_index else {
    return Ok(Some(if sign.is_sign_negative() { -0.0 } else { 0.0 }));
  };

  let dot_index = dot_index.unwrap_or(digits_seen);
  let exp10 = (dot_index as i64)
    .saturating_sub(first_sig_index as i64)
    .saturating_sub(1)
    .saturating_add(exp_part as i64);

  debug_assert!(!sig_digits.is_empty());

  let mut s = String::with_capacity(1 + sig_digits.len() + 2 + 24);
  if sign.is_sign_negative() {
    s.push('-');
  }
  s.push(sig_digits[0] as char);
  if sig_digits.len() > 1 {
    s.push('.');
    for &b in &sig_digits[1..] {
      s.push(b as char);
    }
  }
  s.push('e');
  if exp10 < 0 {
    s.push('-');
    try_write_u32(&mut s, (-exp10) as u32)?;
  } else {
    try_write_u32(&mut s, exp10 as u32)?;
  }

  match fast_float::parse::<f64, _>(&s) {
    Ok(n) => Ok(Some(n)),
    Err(_) => Ok(None),
  }
}

fn is_ecma_whitespace_unit(unit: u16) -> bool {
  matches!(
    unit,
    // WhiteSpace (ECMA-262)
    0x0009
      | 0x000B
      | 0x000C
      | 0x0020
      | 0x00A0
      | 0x1680
      | 0x202F
      | 0x205F
      | 0x3000
      | 0xFEFF
      // LineTerminator (ECMA-262)
      | 0x000A
      | 0x000D
      | 0x2028
      | 0x2029
  ) || matches!(unit, 0x2000..=0x200A)
}

#[allow(dead_code)]
pub(crate) fn is_ecma_whitespace(c: char) -> bool {
  // ECMA-262 WhiteSpace + LineTerminator (used by TrimString / StringToNumber).
  matches!(
    c,
    '\u{0009}' // Tab
    | '\u{000A}' // LF
    | '\u{000B}' // VT
    | '\u{000C}' // FF
    | '\u{000D}' // CR
    | '\u{0020}' // Space
    | '\u{00A0}' // No-break space
    | '\u{1680}' // Ogham space mark
    | '\u{2000}'..='\u{200A}' // En quad..hair space
    | '\u{2028}' // Line separator
    | '\u{2029}' // Paragraph separator
    | '\u{202F}' // Narrow no-break space
    | '\u{205F}' // Medium mathematical space
    | '\u{3000}' // Ideographic space
    | '\u{FEFF}' // BOM
  )
}
