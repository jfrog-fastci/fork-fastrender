use crate::{GcString, Heap, Value, VmError};

/// ECMAScript `ToNumber` for the supported value types.
pub fn to_number(heap: &mut Heap, value: Value) -> Result<f64, VmError> {
  match value {
    Value::Undefined => Ok(f64::NAN),
    Value::Null => Ok(0.0),
    Value::Bool(b) => Ok(if b { 1.0 } else { 0.0 }),
    Value::Number(n) => Ok(n),
    Value::BigInt(_) => Err(VmError::TypeError(
      "Cannot convert a BigInt value to a number",
    )),
    Value::String(s) => string_to_number(heap, s),
    Value::Symbol(_) => Err(VmError::TypeError("Cannot convert a Symbol value to a number")),
    // Per spec, `ToNumber` for objects requires `ToPrimitive`, which can invoke user code.
    // Use `Scope::to_number` in evaluator/built-in call sites where a `Vm` + host context exists.
    Value::Object(_) => Err(VmError::Unimplemented(
      "ToNumber on objects requires ToPrimitive (use Scope::to_number)",
    )),
  }
}

fn string_to_number(heap: &Heap, s: GcString) -> Result<f64, VmError> {
  let raw = heap.get_string(s)?.to_utf8_lossy();
  let trimmed = raw.trim_matches(is_ecma_whitespace);

  if trimmed.is_empty() {
    return Ok(0.0);
  }

  // Infinity is case-sensitive in ECMAScript string numeric literals.
  match trimmed {
    "Infinity" | "+Infinity" => return Ok(f64::INFINITY),
    "-Infinity" => return Ok(f64::NEG_INFINITY),
    _ => {}
  }

  // Guard against Rust accepting "inf"/"infinity" case-insensitively.
  let (has_sign, rest) = match trimmed.strip_prefix('+') {
    Some(rest) => (true, rest),
    None => match trimmed.strip_prefix('-') {
      Some(rest) => (true, rest),
      None => (false, trimmed),
    },
  };
  if rest.eq_ignore_ascii_case("inf") || rest.eq_ignore_ascii_case("infinity") {
    // Only the exact "Infinity" spelling is accepted above.
    return Ok(f64::NAN);
  }

  // Per ECMA-262, signed hex/binary/octal literals are not valid `StringToNumber` inputs.
  // E.g. `Number("-0x10")` is `NaN` (use `parseInt` for signed radix parsing).
  if has_sign {
    if rest.starts_with("0x")
      || rest.starts_with("0X")
      || rest.starts_with("0b")
      || rest.starts_with("0B")
      || rest.starts_with("0o")
      || rest.starts_with("0O")
    {
      return Ok(f64::NAN);
    }
  }

  if let Some(hex) = trimmed.strip_prefix("0x").or_else(|| trimmed.strip_prefix("0X")) {
    return Ok(parse_ascii_int_radix(hex, 16).unwrap_or(f64::NAN));
  }
  if let Some(bin) = trimmed.strip_prefix("0b").or_else(|| trimmed.strip_prefix("0B")) {
    return Ok(parse_ascii_int_radix(bin, 2).unwrap_or(f64::NAN));
  }
  if let Some(oct) = trimmed.strip_prefix("0o").or_else(|| trimmed.strip_prefix("0O")) {
    return Ok(parse_ascii_int_radix(oct, 8).unwrap_or(f64::NAN));
  }

  Ok(trimmed.parse::<f64>().unwrap_or(f64::NAN))
}

fn parse_ascii_int_radix(s: &str, radix: u32) -> Option<f64> {
  if s.is_empty() {
    return None;
  }
  let radix_f = radix as f64;
  let mut value = 0.0f64;
  for b in s.bytes() {
    let digit = match b {
      b'0'..=b'9' => (b - b'0') as u32,
      b'a'..=b'f' => (b - b'a' + 10) as u32,
      b'A'..=b'F' => (b - b'A' + 10) as u32,
      _ => return None,
    };
    if digit >= radix {
      return None;
    }
    value = value * radix_f + digit as f64;
  }
  Some(value)
}

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
