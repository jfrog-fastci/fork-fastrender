use ryu::Buffer;

/// JavaScript-style `Number.prototype.toString()` formatting (base 10).
///
/// This intentionally differs from Rust's `f64::to_string()` formatting:
/// - `-0` formats as `"0"`.
/// - Exponential notation is used for magnitudes `>= 1e21` and `< 1e-6`.
/// - Positive exponents always include a `+` sign (`"1e+21"`).
///
/// The output is deterministic for identical `f64` bit patterns.
pub(crate) fn js_number_to_string(value: f64) -> String {
  // Match JS `Number.prototype.toString()` for non-finite numbers. These should
  // not arise from TS numeric literal syntax, but handling them keeps this helper
  // robust and makes failures easier to interpret.
  if value.is_nan() {
    return "NaN".to_string();
  }
  if value.is_infinite() {
    return if value.is_sign_negative() {
      "-Infinity".to_string()
    } else {
      "Infinity".to_string()
    };
  }

  // JS prints both `0` and `-0` as "0".
  if value == 0.0 {
    return "0".to_string();
  }

  if value.is_sign_negative() {
    return format!("-{}", js_number_to_string(-value));
  }

  let (digits, k) = decimal_digits_and_k(value);
  format_decimal_digits(&digits, k)
}

/// Extract `(digits, k)` as described by ECMA-262's `NumberToString` algorithm.
///
/// `digits` is a non-empty ASCII string of digits with no leading zeros.
/// `k` is the number of digits in the integer part when formatting in decimal
/// (i.e. the position of the decimal point in `digits`).
fn decimal_digits_and_k(value: f64) -> (String, i32) {
  debug_assert!(value.is_finite());
  debug_assert!(value > 0.0);

  let mut buffer = Buffer::new();
  let s = buffer.format_finite(value);

  let (mut mantissa, exp10) = s
    .split_once('e')
    .or_else(|| s.split_once('E'))
    .map(|(m, e)| (m, e.parse::<i32>().expect("ryu exponent should be valid")))
    .unwrap_or((s, 0));

  // `ryu` may emit ".0" for whole numbers in some cases; strip it so the final
  // output matches JS's `Number.prototype.toString()`.
  mantissa = mantissa.strip_suffix(".0").unwrap_or(mantissa);

  // Collect all digits from the mantissa while tracking where the decimal point
  // was.
  let mut all_digits = String::with_capacity(mantissa.len());
  let mut decimal_pos = 0usize;
  let mut saw_decimal = false;
  for b in mantissa.bytes() {
    if b == b'.' {
      decimal_pos = all_digits.len();
      saw_decimal = true;
    } else {
      all_digits.push(b as char);
    }
  }
  if !saw_decimal {
    decimal_pos = all_digits.len();
  }

  // Remove leading zeros (the input is non-zero, so there will always be a
  // non-zero digit somewhere).
  let bytes = all_digits.as_bytes();
  let mut leading_zeros = 0usize;
  while leading_zeros < bytes.len() && bytes[leading_zeros] == b'0' {
    leading_zeros += 1;
  }
  let digits = all_digits[leading_zeros..].to_string();

  let k = (decimal_pos as i32) - (leading_zeros as i32) + exp10;
  (digits, k)
}

fn format_decimal_digits(digits: &str, k: i32) -> String {
  debug_assert!(!digits.is_empty());
  debug_assert!(digits.as_bytes()[0].is_ascii_digit());
  debug_assert!(digits.as_bytes()[0] != b'0');

  let len = digits.len() as i32;

  // Use fixed notation when -6 < k <= 21 (i.e. 1e-6 <= n < 1e21, excluding 0).
  if k <= 21 && k > -6 {
    if k <= 0 {
      let zeros = (-k) as usize;
      let mut out = String::with_capacity(2 + zeros + digits.len());
      out.push_str("0.");
      for _ in 0..zeros {
        out.push('0');
      }
      out.push_str(digits);
      return out;
    }

    if k >= len {
      let zeros = (k - len) as usize;
      let mut out = String::with_capacity(digits.len() + zeros);
      out.push_str(digits);
      for _ in 0..zeros {
        out.push('0');
      }
      return out;
    }

    let k_usize = k as usize;
    let mut out = String::with_capacity(digits.len() + 1);
    out.push_str(&digits[..k_usize]);
    out.push('.');
    out.push_str(&digits[k_usize..]);
    return out;
  }

  // Otherwise, use exponential notation.
  let mut out = String::with_capacity(digits.len() + 6);
  out.push(digits.as_bytes()[0] as char);
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
    out.push_str(&exp.to_string());
  }

  out
}
