//! Minimal helpers for ECMAScript `ToString` behavior.
//!
//! `hir-js` stores numeric literals as strings formatted for *source literals*
//! (via `parse-js`'s `JsNumber` `Display` impl). That formatting is **not**
//! spec-correct for JS `ToString` / `NumberToString`:
//! - `1e21` gets formatted as a long decimal string, but JS uses `"1e+21"`.
//! - `1e400` is a valid numeric literal that evaluates to `Infinity`; JS uses `"Infinity"`.
//!
//! When we treat a computed property key as static (e.g. `obj[1e21]`), we must
//! use the spec `NumberToString` algorithm so the key matches runtime semantics.

/// Convert a parsed JS number value into the spec `NumberToString` result.
///
/// https://tc39.es/ecma262/multipage/ecmascript-data-types-and-values.html#sec-numeric-types-number-tostring
pub(crate) fn number_to_js_string(value: f64) -> String {
  if value.is_nan() {
    return "NaN".to_string();
  }
  if value == 0.0 {
    // Covers both +0 and -0.
    return "0".to_string();
  }
  if value.is_infinite() {
    if value.is_sign_negative() {
      return "-Infinity".to_string();
    } else {
      return "Infinity".to_string();
    }
  }

  let sign = if value.is_sign_negative() { "-" } else { "" };
  let abs = value.abs();

  // Use `ryu` only to get the digit + exponent decomposition; the final formatting rules match
  // ECMAScript `Number::toString()` (not Rust's float formatting).
  let mut buffer = ryu::Buffer::new();
  let raw = buffer.format_finite(abs);
  // `ryu` formats `1.0` as `"1.0"`, but ECMAScript `ToString(1)` is `"1"`.
  let raw = raw.strip_suffix(".0").unwrap_or(raw);
  let (digits, exp) = parse_ryu_to_decimal(raw);
  let k = exp + digits.len() as i32;

  let mut out = String::new();
  out.push_str(sign);

  if k > 0 && k <= 21 {
    let k = k as usize;
    if k >= digits.len() {
      out.push_str(&digits);
      out.extend(std::iter::repeat('0').take(k - digits.len()));
    } else {
      out.push_str(&digits[..k]);
      out.push('.');
      out.push_str(&digits[k..]);
    }
    return out;
  }

  if k <= 0 && k > -6 {
    out.push_str("0.");
    out.extend(std::iter::repeat('0').take((-k) as usize));
    out.push_str(&digits);
    return out;
  }

  // Exponential form.
  let first = digits.as_bytes()[0] as char;
  out.push(first);
  if digits.len() > 1 {
    out.push('.');
    out.push_str(&digits[1..]);
  }
  out.push('e');
  let exp = k - 1;
  if exp >= 0 {
    out.push('+');
    out.push_str(&exp.to_string());
  } else {
    out.push('-');
    out.push_str(&(-exp).to_string());
  }
  out
}

fn parse_ryu_to_decimal(raw: &str) -> (String, i32) {
  // `raw` is expected to be ASCII and contain either:
  // - digits with optional decimal point
  // - digits with optional decimal point and a trailing `e[+-]?\d+`
  //
  // Returns `(digits, exp)` such that `value = digits × 10^exp` and `digits` contains no leading
  // zeros.
  let (mantissa, exp_part) = match raw.split_once('e') {
    Some((mantissa, exp)) => (mantissa, Some(exp)),
    None => (raw, None),
  };

  let mut exp: i32 = exp_part.map_or(0, |e| e.parse().unwrap_or(0));

  let mut digits = String::with_capacity(mantissa.len());
  if let Some((int_part, frac_part)) = mantissa.split_once('.') {
    digits.push_str(int_part);
    digits.push_str(frac_part);
    exp -= frac_part.len() as i32;
  } else {
    digits.push_str(mantissa);
  }

  // Strip leading zeros introduced by `0.xxx` forms.
  let trimmed = digits.trim_start_matches('0');
  // `raw` comes from a non-zero number, so we should always have digits.
  (trimmed.to_string(), exp)
}

/// Convert the `hir-js` number-literal string format into the spec `NumberToString` result.
///
/// This should only be used for runtime stringification semantics (property keys, `ToString`, etc),
/// not for displaying/printing a numeric literal back to source.
pub(crate) fn number_literal_to_js_string(literal: &str) -> String {
  match literal.parse::<f64>() {
    Ok(value) => number_to_js_string(value),
    // Should never happen (HIR number strings come from parsed numbers), but avoid dropping
    // resolution if an unexpected string leaks through.
    Err(_) => literal.to_string(),
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn number_to_js_string_matches_ecma262_exponent_rules() {
    // `NumberToString` uses decimal form for 1e20 but exponential form for 1e21.
    assert_eq!(number_to_js_string(1e20), "100000000000000000000");
    assert_eq!(number_to_js_string(1e21), "1e+21");
    assert_eq!(number_to_js_string(1e-6), "0.000001");
    assert_eq!(number_to_js_string(1e-7), "1e-7");
  }

  #[test]
  fn number_literal_to_js_string_handles_hir_number_formatting() {
    // `hir-js` stores number literals using `JsNumber` formatting (literal-ish), not JS `ToString`.
    // These are common cases where the formatting differs.
    assert_eq!(
      number_literal_to_js_string("1000000000000000000000"),
      "1e+21"
    );
    assert_eq!(number_literal_to_js_string("0.0000001"), "1e-7");
    assert_eq!(number_literal_to_js_string("1e400"), "Infinity");
    assert_eq!(number_literal_to_js_string("-0"), "0");
  }
}
