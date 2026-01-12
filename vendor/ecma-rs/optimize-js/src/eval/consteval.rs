use crate::il::inst::BinOp;
use crate::il::inst::BinOp::*;
use crate::il::inst::Const;
use crate::il::inst::Const::*;
use crate::il::inst::UnOp;
use crate::il::inst::UnOp::*;
use num_bigint::{BigInt, Sign};
use num_traits::{ToPrimitive, Zero};
use parse_js::char::{ECMASCRIPT_LINE_TERMINATORS, ECMASCRIPT_WHITESPACE};
use parse_js::num::JsNumber as JN;
use std::cmp::Ordering;
use std::f64::consts::{E, FRAC_1_SQRT_2, LN_10, LN_2, LOG10_E, LOG2_E, PI, SQRT_2};
use std::mem::discriminant;

// To avoid compile-time resource exhaustion, we cap BigInt operations that can rapidly amplify a
// small literal into an enormous value (e.g. `1n << 10_000_000n` or `2n ** 10_000_000n`). When the
// output could exceed this size, we skip folding and preserve runtime semantics.
const BIGINT_MAX_RESULT_BITS: usize = 1 << 20; // 1,048,576 bits (~128 KiB)

/**
 * NOTES ON BUILTINS
 *
 * We often intentionally skip const evaluating builtin values (i.e. at least one arg is a Arg::Builtin). Their values are opaque to us.
 * Yes technically we have the list of all builtins. But we may have forgotten some or new ones may be added in the future and we haven't implemented them yet (and our compiler shouldn't emit incorrect code even then). We don't want to give an incorrect answer.
 * Note that all of these are unsafe:
 * - Checking if they strictly equal. Even if paths are identical, they could point to NaN (e.g. `Number.NaN`); even if paths are unidentical, they could still point to the same object (e.g. `Number.POSITIVE_INFINITY` and `Infinity`). It's incorrect to return either true or false, because there are exceptions in both cases. And these exceptions could change in the future, even if our compiler doesn't, but our compiler still has to be correct then.
 * - Checking if either is null or undefined. A builtin could be null or undefined. Accessing an unknown property on a builtin object results in undefined *today* but may not in the future.
 * - Even `void (Builtin)` is not safe because the builtin path may not exist and we could be suppressing an error.
 */
fn is_ecmascript_whitespace(ch: char) -> bool {
  ECMASCRIPT_WHITESPACE.contains(&ch) || ECMASCRIPT_LINE_TERMINATORS.contains(&ch)
}

fn trim_js_whitespace(raw: &str) -> &str {
  raw.trim_matches(is_ecmascript_whitespace)
}

fn bigint_to_f64(value: &BigInt) -> f64 {
  value.to_f64().unwrap_or_else(|| {
    if value.sign() == Sign::Minus {
      f64::NEG_INFINITY
    } else {
      f64::INFINITY
    }
  })
}

fn bigint_from_integral_f64(value: f64) -> BigInt {
  debug_assert!(value.is_finite());
  debug_assert!(value.trunc() == value);

  if value == 0.0 {
    return BigInt::from(0);
  }

  let bits = value.to_bits();
  let sign = (bits >> 63) != 0;
  let exp_bits = ((bits >> 52) & 0x7ff) as i32;
  let frac_bits = bits & ((1u64 << 52) - 1);

  // Subnormals are always in (-1, 1), so the only integral subnormal is 0.0 (handled above).
  debug_assert!(exp_bits != 0);

  let exp = exp_bits - 1023;
  let mantissa = frac_bits | (1u64 << 52);
  // `mantissa` is interpreted with 52 fractional bits.
  let shift = exp - 52;

  let mut result = BigInt::from(mantissa);
  if shift >= 0 {
    result <<= shift as usize;
  } else {
    result >>= (-shift) as usize;
  }

  if sign { -result } else { result }
}

fn bigint_abs_bit_info(value: &BigInt) -> (usize, Option<usize>) {
  // Returns `(bit_length, pow2_log2)` for `abs(value)`.
  //
  // `pow2_log2` is `Some(k)` when `abs(value) == 2^k`, otherwise `None`.
  let (_, bytes) = value.to_bytes_le();
  if bytes.is_empty() {
    return (0, None);
  }

  let last = *bytes.last().unwrap();
  let bit_len = bytes.len() * 8 - (last.leading_zeros() as usize);

  let mut pow2: Option<(usize, u8)> = None;
  for (i, &b) in bytes.iter().enumerate() {
    if b == 0 {
      continue;
    }
    if pow2.is_some() {
      pow2 = None;
      break;
    }
    if (b & (b - 1)) != 0 {
      pow2 = None;
      break;
    }
    pow2 = Some((i, b));
  }

  let pow2_log2 = pow2.map(|(i, b)| i * 8 + (b.trailing_zeros() as usize));
  (bit_len, pow2_log2)
}

fn bigint_is_odd(value: &BigInt) -> bool {
  let (_, bytes) = value.to_bytes_le();
  bytes.first().is_some_and(|b| (b & 1) == 1)
}

fn coerce_to_index(v: &Const) -> Option<u64> {
  // https://tc39.es/ecma262/multipage/abstract-operations.html#sec-toindex
  //
  // `ToIndex` is basically `ToIntegerOrInfinity(ToNumber(x))`, clamped to:
  //   0 <= n <= 2^53 - 1
  // and throwing on ±∞ or out-of-range values.
  if matches!(v, BigInt(_)) {
    // `ToNumber(1n)` throws a TypeError.
    return None;
  }
  let n = coerce_to_num(v);
  if n.is_nan() {
    return Some(0);
  }
  if !n.is_finite() {
    return None;
  }
  let int = n.trunc();
  if int == 0.0 {
    // Covers both +0 and -0 (and negative fractional values in (-1, 0)).
    return Some(0);
  }
  if int < 0.0 {
    return None;
  }
  // `ToIndex` rejects values larger than `Number.MAX_SAFE_INTEGER`.
  if int > 9007199254740991.0 {
    return None;
  }
  Some(int as u64)
}

fn coerce_to_bigint_for_bigint_bitop(v: &Const) -> Option<BigInt> {
  // `BigInt.asIntN` / `BigInt.asUintN` use `ToBigInt`, which *does not* accept numbers.
  match v {
    BigInt(v) => Some(v.clone()),
    Bool(v) => Some(BigInt::from(*v as u8)),
    Str(v) => parse_bigint(v),
    _ => None,
  }
}

fn bigint_fits_signed_bits(value: &BigInt, bits: u64) -> bool {
  if bits == 0 {
    return value.is_zero();
  }
  if value.is_zero() {
    return true;
  }

  let boundary = bits - 1;
  if value.sign() != Sign::Minus {
    let (bit_len, _) = bigint_abs_bit_info(value);
    return (bit_len as u64) <= boundary;
  }

  let (abs_bit_len, pow2_log2) = bigint_abs_bit_info(value);
  if (abs_bit_len as u64) <= boundary {
    return true;
  }
  pow2_log2.is_some_and(|k| k as u64 == boundary)
}

fn bigint_as_uint_n(bits: u64, value: &BigInt) -> Option<BigInt> {
  if bits == 0 {
    return Some(BigInt::from(0));
  }

  // Fast path: if the number is non-negative and already fits in `bits`, the result is itself.
  if value.sign() != Sign::Minus {
    let (bit_len, _) = bigint_abs_bit_info(value);
    if bits >= bit_len as u64 {
      return Some(value.clone());
    }
  }

  // Otherwise we'd need to materialize a `2^bits - 1` mask. Avoid huge allocations.
  if bits > BIGINT_MAX_RESULT_BITS as u64 {
    return None;
  }
  let bits = bits as usize;
  let mask = (BigInt::from(1) << bits) - 1;
  Some(value & mask)
}

fn bigint_as_int_n(bits: u64, value: &BigInt) -> Option<BigInt> {
  if bits == 0 {
    return Some(BigInt::from(0));
  }

  // Fast path: if the value already fits the signed `bits` range, the conversion is a no-op.
  if bigint_fits_signed_bits(value, bits) {
    return Some(value.clone());
  }

  if bits > BIGINT_MAX_RESULT_BITS as u64 {
    return None;
  }
  let bits = bits as usize;
  let mask = (BigInt::from(1) << bits) - 1;
  let unsigned = value & &mask;
  let sign_bit = BigInt::from(1) << (bits - 1);
  if unsigned >= sign_bit {
    Some(unsigned - (BigInt::from(1) << bits))
  } else {
    Some(unsigned)
  }
}

fn parse_int_digits_to_bigint(digits: &str, radix: u32) -> Option<BigInt> {
  if digits.is_empty() || digits.contains('_') {
    return None;
  }
  if !digits.chars().all(|ch| ch.to_digit(radix).is_some()) {
    return None;
  }
  BigInt::parse_bytes(digits.as_bytes(), radix)
}

pub fn parse_bigint(raw: &str) -> Option<BigInt> {
  let trimmed = trim_js_whitespace(raw);
  if trimmed.is_empty() {
    // `BigInt("")` and `BigInt("   ")` evaluate to `0n`.
    return Some(BigInt::from(0));
  }
  let mut sign = 1;
  let mut had_sign = false;
  let mut body = trimmed;
  if let Some(rest) = body.strip_prefix('+') {
    had_sign = true;
    body = rest;
  } else if let Some(rest) = body.strip_prefix('-') {
    had_sign = true;
    sign = -1;
    body = rest;
  }
  if body.is_empty() {
    return None;
  }
  let (radix, digits, had_prefix) = match body {
    s if s.starts_with("0b") || s.starts_with("0B") => (2, &s[2..], true),
    s if s.starts_with("0o") || s.starts_with("0O") => (8, &s[2..], true),
    s if s.starts_with("0x") || s.starts_with("0X") => (16, &s[2..], true),
    s => (10, s, false),
  };
  // `BigInt("0xF")` is accepted, but `BigInt("-0xF")` / `BigInt("+0xF")` throw.
  if had_prefix && had_sign {
    return None;
  }
  let mut value = parse_int_digits_to_bigint(digits, radix)?;
  if sign == -1 {
    value = -value;
  }
  Some(value)
}

pub fn coerce_bigint_to_num(v: &BigInt) -> f64 {
  bigint_to_f64(v)
}

fn bigint_number_loose_eq(big: &BigInt, num: f64) -> bool {
  // https://tc39.es/ecma262/multipage/abstract-operations.html#sec-islooselyequal
  // BigInt/Number equality uses `BigInt::equal`:
  // - NaN / ±∞ => false
  // - Non-integer numbers => false
  // - Otherwise compare with the *exact* integer value represented by the IEEE-754 double.
  if !num.is_finite() {
    return false;
  }
  if num.trunc() != num {
    return false;
  }
  big == &bigint_from_integral_f64(num)
}

fn bigint_number_cmp(big: &BigInt, num: f64) -> Option<Ordering> {
  if num.is_nan() {
    return None;
  }
  if num.is_infinite() {
    return Some(if num.is_sign_positive() {
      Ordering::Less
    } else {
      Ordering::Greater
    });
  }

  if num.trunc() == num {
    return Some(big.cmp(&bigint_from_integral_f64(num)));
  }

  // `num` is finite and non-integral. Since `big` is an integer, `big < num` is equivalent to
  // `big <= floor(num)`.
  let floor = bigint_from_integral_f64(num.floor());
  Some(if big <= &floor {
    Ordering::Less
  } else {
    Ordering::Greater
  })
}

// https://developer.mozilla.org/en-US/docs/Web/JavaScript/Reference/Global_Objects/Number#number_coercion
pub fn coerce_str_to_num(raw: &str) -> f64 {
  let raw = trim_js_whitespace(raw);
  if raw.is_empty() {
    return 0.0;
  };
  let mut sign = 1.0_f64;
  let mut had_sign = false;
  let mut body = raw;
  if let Some(rest) = body.strip_prefix('+') {
    had_sign = true;
    body = rest;
  } else if let Some(rest) = body.strip_prefix('-') {
    had_sign = true;
    sign = -1.0;
    body = rest;
  };
  if body.is_empty() {
    return f64::NAN;
  };
  if body == "Infinity" {
    return sign * f64::INFINITY;
  };

  let parse_int =
    |digits: &str, radix: u32| parse_int_digits_to_bigint(digits, radix).map(|v| bigint_to_f64(&v));

  if body.starts_with("0x") || body.starts_with("0X") {
    if had_sign {
      return f64::NAN;
    }
    return parse_int(&body[2..], 16).unwrap_or(f64::NAN);
  }
  if body.starts_with("0b") || body.starts_with("0B") {
    if had_sign {
      return f64::NAN;
    }
    return parse_int(&body[2..], 2).unwrap_or(f64::NAN);
  }
  if body.starts_with("0o") || body.starts_with("0O") {
    if had_sign {
      return f64::NAN;
    }
    return parse_int(&body[2..], 8).unwrap_or(f64::NAN);
  }

  if body.contains('_') {
    return f64::NAN;
  }

  let mut saw_digit_before_exp = false;
  let mut saw_dot = false;
  let mut saw_exp = false;
  let mut iter = body.chars().peekable();
  while let Some(ch) = iter.next() {
    match ch {
      '0'..='9' => {
        if !saw_exp {
          saw_digit_before_exp = true;
        }
      }
      '.' => {
        if saw_dot || saw_exp {
          return f64::NAN;
        }
        saw_dot = true;
      }
      'e' | 'E' => {
        if saw_exp || !saw_digit_before_exp {
          return f64::NAN;
        }
        saw_exp = true;
        if matches!(iter.peek(), Some('+' | '-')) {
          iter.next();
        }
        let mut exp_digits = 0;
        while matches!(iter.peek(), Some('0'..='9')) {
          exp_digits += 1;
          iter.next();
        }
        if exp_digits == 0 {
          return f64::NAN;
        }
      }
      _ => return f64::NAN,
    }
  }
  if !saw_digit_before_exp {
    return f64::NAN;
  }

  body.parse::<f64>().map(|v| sign * v).unwrap_or(f64::NAN)
}

// https://tc39.es/ecma262/multipage/abstract-operations.html#sec-tonumber
pub fn coerce_to_num(v: &Const) -> f64 {
  match v {
    BigInt(_) => panic!("cannot coerce bigint to num according to spec"),
    Bool(false) => 0.0,
    Bool(true) => 1.0,
    Null => 0.0,
    Num(v) => v.0,
    Str(v) => coerce_str_to_num(&v),
    Undefined => f64::NAN,
  }
}

// https://tc39.es/ecma262/multipage/abstract-operations.html#sec-toint32
fn coerce_to_uint32(v: &Const) -> Option<u32> {
  if matches!(v, BigInt(_)) {
    return None;
  }
  let n = coerce_to_num(v);
  if !n.is_finite() || n == 0.0 {
    return Some(0);
  }
  let int = n.trunc();
  let wrapped = int.rem_euclid(4294967296.0);
  Some(wrapped as u32)
}

fn coerce_to_int32(v: &Const) -> Option<i32> {
  coerce_to_uint32(v).map(|v| v as i32)
}

// https://developer.mozilla.org/en-US/docs/Glossary/Falsy
pub fn coerce_to_bool(v: &Const) -> bool {
  match v {
    BigInt(v) => !v.is_zero(),
    Bool(b) => *b,
    Null => false,
    Num(JN(v)) => !v.is_nan() && *v != 0.0,
    Str(v) => !v.is_empty(),
    Undefined => false,
  }
}

fn number_to_js_string(value: f64) -> String {
  // https://tc39.es/ecma262/multipage/ecmascript-data-types-and-values.html#sec-numeric-types-number-tostring
  //
  // This mirrors the minimal `Number::toString` implementation in `vm-js`. Using `JsNumber`'s
  // `Display` formatting is incorrect here because it formats numeric *literals* (e.g. `Infinity` as
  // `1e400`) rather than JS `ToString` (`"Infinity"`).
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

fn const_to_js_string(value: &Const) -> String {
  match value {
    BigInt(v) => v.to_string(),
    Bool(true) => "true".into(),
    Bool(false) => "false".into(),
    Null => "null".into(),
    Num(v) => number_to_js_string(v.0),
    Str(v) => v.clone(),
    Undefined => "undefined".into(),
  }
}

fn js_string_cmp(a: &str, b: &str) -> Ordering {
  let mut a_units = a.encode_utf16();
  let mut b_units = b.encode_utf16();
  loop {
    match (a_units.next(), b_units.next()) {
      (None, None) => return Ordering::Equal,
      (None, Some(_)) => return Ordering::Less,
      (Some(_), None) => return Ordering::Greater,
      (Some(a), Some(b)) => {
        if a != b {
          return a.cmp(&b);
        }
      }
    }
  }
}

// If return value is None, then all comparison operators between `a` and `b` result in false.
// https://tc39.es/ecma262/multipage/abstract-operations.html#sec-islessthan
pub fn js_cmp(a: &Const, b: &Const) -> Option<Ordering> {
  match (a, b) {
    (Str(a), Str(b)) => Some(js_string_cmp(a, b)),
    (BigInt(a), BigInt(b)) => Some(a.cmp(b)),
    (BigInt(a), b) => bigint_number_cmp(a, coerce_to_num(b)),
    (a, BigInt(b)) => bigint_number_cmp(b, coerce_to_num(a)).map(Ordering::reverse),
    (a, b) => {
      // https://tc39.es/ecma262/multipage/ecmascript-data-types-and-values.html#sec-numeric-types-number-lessThan
      let a = coerce_to_num(a);
      let b = coerce_to_num(b);
      if a.is_nan() || b.is_nan() {
        None
      } else {
        Some(a.partial_cmp(&b).unwrap())
      }
    }
  }
}

pub fn js_div(a: f64, b: f64) -> f64 {
  a / b
}

pub fn js_round(value: f64) -> f64 {
  // https://tc39.es/ecma262/multipage/numbers-and-dates.html#sec-math.round
  //
  // Rust's `f64::round` rounds ties away from 0, but ECMAScript `Math.round` rounds ties toward
  // +∞ (e.g. `Math.round(-1.5) === -1`) and preserves `-0` for negative inputs in (-0.5, 0).
  let rounded = (value + 0.5).floor();
  if rounded == 0.0 && value.is_sign_negative() {
    -0.0
  } else {
    rounded
  }
}

pub fn js_sign(value: f64) -> f64 {
  // https://tc39.es/ecma262/multipage/numbers-and-dates.html#sec-math.sign
  if value.is_nan() {
    f64::NAN
  } else if value == 0.0 {
    // Preserve +0 / -0.
    value
  } else if value.is_sign_negative() {
    -1.0
  } else {
    1.0
  }
}

pub fn js_fround(value: f64) -> f64 {
  // https://tc39.es/ecma262/multipage/numbers-and-dates.html#sec-math.fround
  // Convert via `f32` to apply IEEE-754 single-precision rounding. This preserves `-0` and maps all
  // NaN payloads to a canonical NaN, which is fine for ECMAScript observable semantics.
  (value as f32) as f64
}

pub fn js_mod(a: f64, b: f64) -> f64 {
  match (a, b) {
    (_, 0.0) => f64::NAN,
    (a, _) if a.is_infinite() => f64::NAN,
    _ => a % b,
  }
}

// https://tc39.es/ecma262/multipage/abstract-operations.html#sec-islooselyequal
pub fn js_loose_eq(a: &Const, b: &Const) -> bool {
  if discriminant(a) == discriminant(b) {
    return js_strict_eq(a, b);
  };
  match (a, b) {
    (Null, Undefined) => true,
    (Undefined, Null) => true,
    (Num(l), Str(r)) => l.0 == coerce_str_to_num(&r),
    (Str(l), Num(r)) => coerce_str_to_num(&l) == r.0,
    (BigInt(l), Str(r)) => parse_bigint(r).is_some_and(|r| l == &r),
    (Str(l), BigInt(r)) => parse_bigint(l).is_some_and(|l| &l == r),
    (Bool(l), r) => js_loose_eq(&Num(JN(*l as u8 as f64)), r),
    (l, Bool(r)) => js_loose_eq(l, &Num(JN(*r as u8 as f64))),
    (BigInt(l), Num(r)) => bigint_number_loose_eq(l, r.0),
    (Num(l), BigInt(r)) => bigint_number_loose_eq(r, l.0),
    _ => false,
  }
}

pub fn js_strict_eq(a: &Const, b: &Const) -> bool {
  match (a, b) {
    (Num(v), _) if v.0.is_nan() => false,
    (_, Num(v)) if v.0.is_nan() => false,
    (a, b) => a == b,
  }
}

pub fn maybe_eval_const_bin_expr(op: BinOp, a: &Const, b: &Const) -> Option<Const> {
  #[rustfmt::skip]
  let res = match (op, a, b) {
    (Add, BigInt(l), BigInt(r)) => BigInt(l + r),
    (Add, Num(l), Num(r)) => Num(JN(l.0 + r.0)),
    (Add, Str(l), r) => {
      let rhs = const_to_js_string(r);
      let mut out = String::with_capacity(l.len() + rhs.len());
      out.push_str(l);
      out.push_str(&rhs);
      Str(out)
    },
    (Add, l, Str(r)) => {
      let lhs = const_to_js_string(l);
      let mut out = String::with_capacity(lhs.len() + r.len());
      out.push_str(&lhs);
      out.push_str(r);
      Str(out)
    },
    (BitAnd, BigInt(l), BigInt(r)) => BigInt(l & r),
    (Div, Num(l), Num(r)) => Num(JN(js_div(l.0, r.0))),
    (Div, Num(l), Str(r)) => Num(JN(js_div(l.0, coerce_str_to_num(r)))),
    (Div, Str(l), Num(r)) => Num(JN(js_div(coerce_str_to_num(l), r.0))),
    (Div, Str(l), Str(r)) => Num(JN(js_div(coerce_str_to_num(l), coerce_str_to_num(r)))),
    (Div, BigInt(_), BigInt(r)) if r.is_zero() => return None,
    (Div, BigInt(l), BigInt(r)) => BigInt(l / r),
    (BitAnd, l, r) => Num(JN((coerce_to_int32(l)? & coerce_to_int32(r)?) as f64)),
    (BitOr, BigInt(l), BigInt(r)) => BigInt(l | r),
    (BitOr, l, r) => Num(JN((coerce_to_int32(l)? | coerce_to_int32(r)?) as f64)),
    (BitXor, BigInt(l), BigInt(r)) => BigInt(l ^ r),
    (BitXor, l, r) => Num(JN((coerce_to_int32(l)? ^ coerce_to_int32(r)?) as f64)),
    (Exp, BigInt(l), BigInt(r)) => {
      if r.sign() == Sign::Minus {
        // `2n ** -1n` throws a RangeError.
        return None;
      }
      if r.is_zero() {
        BigInt(BigInt::from(1))
      } else if l.is_zero() {
        BigInt(BigInt::from(0))
      } else if l == &BigInt::from(1) {
        BigInt(BigInt::from(1))
      } else if l == &BigInt::from(-1) {
        if bigint_is_odd(r) { BigInt(BigInt::from(-1)) } else { BigInt(BigInt::from(1)) }
      } else {
        let exp = r.to_u32()?;
        let (base_bits, pow2_log2) = bigint_abs_bit_info(l);
        let est_bits = match pow2_log2 {
          Some(k) => k.saturating_mul(exp as usize).saturating_add(1),
          None => base_bits.saturating_mul(exp as usize).saturating_add(1),
        };
        if est_bits > BIGINT_MAX_RESULT_BITS {
          return None;
        }
        BigInt(l.pow(exp))
      }
    }
    (Exp, Num(l), Num(r)) => Num(JN(l.0.powf(r.0))),
    (Exp, Num(l), Str(r)) => Num(JN(l.0.powf(coerce_str_to_num(r)))),
    (Exp, Str(l), Num(r)) => Num(JN(coerce_str_to_num(l).powf(r.0))),
    (Exp, Str(l), Str(r)) => Num(JN(coerce_str_to_num(l).powf(coerce_str_to_num(r)))),
    (Geq, l, r) => Bool(js_cmp(l, r).is_some_and(|c| c.is_ge())),
    (Gt, l, r) => Bool(js_cmp(l, r).is_some_and(|c| c.is_gt())),
    (Leq, l, r) => Bool(js_cmp(l, r).is_some_and(|c| c.is_le())),
    (LooseEq, l, r) => Bool(js_loose_eq(l, r)),
    (Lt, l, r) => Bool(js_cmp(l, r).is_some_and(|c| c.is_lt())),
    (Mod, BigInt(_), BigInt(r)) if r.is_zero() => return None,
    (Mod, BigInt(l), BigInt(r)) => BigInt(l % r),
    (Mod, Num(l), Num(r)) => Num(JN(js_mod(l.0, r.0))),
    (Mod, Num(l), Str(r)) => Num(JN(js_mod(l.0, coerce_str_to_num(r)))),
    (Mod, Str(l), Num(r)) => Num(JN(js_mod(coerce_str_to_num(l), r.0))),
    (Mod, Str(l), Str(r)) => Num(JN(js_mod(coerce_str_to_num(l), coerce_str_to_num(r)))),
    (Mul, BigInt(l), BigInt(r)) => BigInt(l * r),
    (Mul, Num(l), Num(r)) => Num(JN(l.0 * r.0)),
    (Mul, Num(l), Str(r)) => Num(JN(l.0 * coerce_str_to_num(r))),
    (Mul, Str(l), Num(r)) => Num(JN(coerce_str_to_num(l) * r.0)),
    (Mul, Str(l), Str(r)) => Num(JN(coerce_str_to_num(l) * coerce_str_to_num(r))),
    (NotLooseEq, l, r) => Bool(!js_loose_eq(l, r)),
    (NotStrictEq, l, r) => Bool(!js_strict_eq(l, r)),
    (Shl, BigInt(l), BigInt(r)) => {
      if r.sign() == Sign::Minus {
        return None;
      }
      if l.is_zero() {
        BigInt(BigInt::from(0))
      } else {
        let shift = r.to_usize()?;
        let (lhs_bits, _) = bigint_abs_bit_info(l);
        if lhs_bits.saturating_add(shift) > BIGINT_MAX_RESULT_BITS {
          return None;
        }
        BigInt(l << shift)
      }
    }
    (Shl, l, r) => {
      let shift = (coerce_to_uint32(r)? & 0x1f) as u32;
      Num(JN(coerce_to_int32(l)?.wrapping_shl(shift) as f64))
    }
    (Shr, BigInt(l), BigInt(r)) => {
      if r.sign() == Sign::Minus {
        return None;
      }
      if l.is_zero() {
        BigInt(BigInt::from(0))
      } else if let Some(shift) = r.to_usize() {
        BigInt(l >> shift)
      } else if l.sign() == Sign::Minus {
        BigInt(BigInt::from(-1))
      } else {
        BigInt(BigInt::from(0))
      }
    }
    (Shr, l, r) => {
      let shift = (coerce_to_uint32(r)? & 0x1f) as u32;
      Num(JN(coerce_to_int32(l)?.wrapping_shr(shift) as f64))
    }
    (UShr, BigInt(_), BigInt(_)) => return None,
    (UShr, l, r) => {
      let shift = (coerce_to_uint32(r)? & 0x1f) as u32;
      Num(JN(coerce_to_uint32(l)?.wrapping_shr(shift) as f64))
    }
    (StrictEq, l, r) => Bool(js_strict_eq(l, r)),
    (Sub, BigInt(l), BigInt(r)) => BigInt(l - r),
    (Sub, Num(l), Num(r)) => Num(JN(l.0 - r.0)),
    (Sub, Num(l), Str(r)) => Num(JN(l.0 - coerce_str_to_num(r))),
    (Sub, Str(l), Num(r)) => Num(JN(coerce_str_to_num(l) - r.0)),
    (Sub, Str(l), Str(r)) => Num(JN(coerce_str_to_num(l) - coerce_str_to_num(r))),
    _ => return None,
  };
  Some(res)
}

pub fn maybe_eval_const_un_expr(op: UnOp, a: &Const) -> Option<Const> {
  #[rustfmt::skip]
  let res = match (op, a) {
    (BitNot, BigInt(a)) => BigInt(!a),
    (BitNot, a) => Num(JN((!coerce_to_int32(a)?) as f64)),
    (Neg, BigInt(a)) => BigInt(-a),
    (Neg, Num(l)) => Num(JN(-l.0)),
    (Not, a) => Bool(!coerce_to_bool(a)),
    (Plus, BigInt(_)) => return None,
    (Plus, l) => Num(JN(coerce_to_num(&l))),
    (Typeof, BigInt(_)) => Str("bigint".into()),
    (Typeof, Bool(_)) => Str("boolean".into()),
    (Typeof, Null) => Str("object".into()),
    (Typeof, Num(_)) => Str("number".into()),
    (Typeof, Str(_)) => Str("string".into()),
    (Typeof, Undefined) => Str("undefined".into()),
    (Void, _) => Undefined,
    _ => return None,
  };
  Some(res)
}

pub fn maybe_eval_const_builtin_call(func: &str, args: &[Const]) -> Option<Const> {
  #[rustfmt::skip]
  let v = match args.len() {
    0 => match func {
      "Math.max" => Num(JN(f64::NEG_INFINITY)),
      "Math.min" => Num(JN(f64::INFINITY)),
      _ => return None,
    }
    1 => match (func, &args[0]) {
      ("BigInt", BigInt(v)) => BigInt(v.clone()),
      ("BigInt", Bool(v)) => BigInt(BigInt::from(*v as u8)),
      ("BigInt", Num(v)) => {
        let value = v.0;
        if !value.is_finite() || value.trunc() != value {
          return None;
        }
        BigInt(bigint_from_integral_f64(value))
      }
      ("BigInt", Str(v)) => BigInt(parse_bigint(v)?),
      ("Math.abs", BigInt(_))
      | ("Math.acos", BigInt(_))
      | ("Math.asin", BigInt(_))
      | ("Math.atan", BigInt(_))
      | ("Math.ceil", BigInt(_))
      | ("Math.cos", BigInt(_))
      | ("Math.max", BigInt(_))
      | ("Math.min", BigInt(_))
      | ("Math.floor", BigInt(_))
      | ("Math.fround", BigInt(_))
      | ("Math.log", BigInt(_))
      | ("Math.log10", BigInt(_))
      | ("Math.log1p", BigInt(_))
      | ("Math.log2", BigInt(_))
      | ("Math.round", BigInt(_))
      | ("Math.sign", BigInt(_))
      | ("Math.sin", BigInt(_))
      | ("Math.sqrt", BigInt(_))
      | ("Math.tan", BigInt(_))
      | ("Math.trunc", BigInt(_)) => return None,
      ("Math.abs", a) => Num(JN(coerce_to_num(a).abs())),
      ("Math.acos", a) => Num(JN(coerce_to_num(a).acos())),
      ("Math.asin", a) => Num(JN(coerce_to_num(a).asin())),
      ("Math.atan", a) => Num(JN(coerce_to_num(a).atan())),
      ("Math.ceil", a) => Num(JN(coerce_to_num(a).ceil())),
      ("Math.clz32", a) => Num(JN((coerce_to_uint32(a)?).leading_zeros() as f64)),
      ("Math.cos", a) => Num(JN(coerce_to_num(a).cos())),
      ("Math.floor", a) => Num(JN(coerce_to_num(a).floor())),
      ("Math.fround", a) => Num(JN(js_fround(coerce_to_num(a)))),
      ("Math.log", a) => Num(JN(coerce_to_num(a).ln())),
      ("Math.log10", a) => Num(JN(coerce_to_num(a).log10())),
      ("Math.log1p", a) => Num(JN(coerce_to_num(a).ln_1p())),
      ("Math.log2", a) => Num(JN(coerce_to_num(a).log2())),
      ("Math.max", a) => Num(JN(coerce_to_num(a))),
      ("Math.min", a) => Num(JN(coerce_to_num(a))),
      ("Math.round", a) => Num(JN(js_round(coerce_to_num(a)))),
      ("Math.sign", a) => Num(JN(js_sign(coerce_to_num(a)))),
      ("Math.sin", a) => Num(JN(coerce_to_num(a).sin())),
      ("Math.sqrt", a) => Num(JN(coerce_to_num(a).sqrt())),
      ("Math.tan", a) => Num(JN(coerce_to_num(a).tan())),
      ("Math.trunc", a) => Num(JN(coerce_to_num(a).trunc())),
      ("Number", BigInt(_)) => return None,
      ("Number", a) => Num(JN(coerce_to_num(a))),
      _ => return None,
    }
    2 => match (func, &args[0], &args[1]) {
      ("Math.max", BigInt(_), _) | ("Math.max", _, BigInt(_)) => return None,
      ("Math.min", BigInt(_), _) | ("Math.min", _, BigInt(_)) => return None,
      ("Math.max", a, b) => {
        let a = coerce_to_num(a);
        let b = coerce_to_num(b);
        if a.is_nan() || b.is_nan() {
          Num(JN(f64::NAN))
        } else if a > b {
          Num(JN(a))
        } else if b > a {
          Num(JN(b))
        } else if a == 0.0 {
          // `Math.max(-0, +0) === +0`.
          let both_neg = a.is_sign_negative() && b.is_sign_negative();
          Num(JN(if both_neg { -0.0 } else { 0.0 }))
        } else {
          Num(JN(a))
        }
      }
      ("Math.min", a, b) => {
        let a = coerce_to_num(a);
        let b = coerce_to_num(b);
        if a.is_nan() || b.is_nan() {
          Num(JN(f64::NAN))
        } else if a < b {
          Num(JN(a))
        } else if b < a {
          Num(JN(b))
        } else if a == 0.0 {
          // `Math.min(+0, -0) === -0`.
          let any_neg = a.is_sign_negative() || b.is_sign_negative();
          Num(JN(if any_neg { -0.0 } else { 0.0 }))
        } else {
          Num(JN(a))
        }
      }
      ("Math.pow", BigInt(_), _) | ("Math.pow", _, BigInt(_)) => return None,
      ("Math.pow", base, exp) => Num(JN(coerce_to_num(base).powf(coerce_to_num(exp)))),
      ("Math.imul", a, b) => Num(JN((coerce_to_int32(a)?).wrapping_mul(coerce_to_int32(b)?) as f64)),
      ("BigInt.asIntN", bits, value) => {
        let bits = coerce_to_index(bits)?;
        let value = coerce_to_bigint_for_bigint_bitop(value)?;
        BigInt(bigint_as_int_n(bits, &value)?)
      }
      ("BigInt.asUintN", bits, value) => {
        let bits = coerce_to_index(bits)?;
        let value = coerce_to_bigint_for_bigint_bitop(value)?;
        BigInt(bigint_as_uint_n(bits, &value)?)
      }
      _ => return None,
    }
    _ => match func {
      "Math.max" | "Math.min" => {
        let mut out = if func == "Math.max" { f64::NEG_INFINITY } else { f64::INFINITY };
        for arg in args {
          if matches!(arg, BigInt(_)) {
            return None;
          }
          let v = coerce_to_num(arg);
          if v.is_nan() {
            return Some(Num(JN(f64::NAN)));
          }
          if func == "Math.max" {
            if v > out {
              out = v;
            } else if v == out && v == 0.0 && out.is_sign_negative() && v.is_sign_positive() {
              // `Math.max(-0, +0) === +0`.
              out = v;
            }
          } else {
            if v < out {
              out = v;
            } else if v == out && v == 0.0 && out.is_sign_positive() && v.is_sign_negative() {
              // `Math.min(+0, -0) === -0`.
              out = v;
            }
          }
        }
        Num(JN(out))
      }
      _ => return None,
    },
  };
  Some(v)
}

pub fn maybe_eval_const_builtin_val(path: &str) -> Option<Const> {
  #[rustfmt::skip]
  let v = match path {
    "Infinity" => Num(JN(f64::INFINITY)),
    "Math.E" => Num(JN(E)),
    "Math.LN10" => Num(JN(LN_10)),
    "Math.LN2" => Num(JN(LN_2)),
    "Math.LOG10E" => Num(JN(LOG10_E)),
    "Math.LOG2E" => Num(JN(LOG2_E)),
    "Math.PI" => Num(JN(PI)),
    "Math.SQRT1_2" => Num(JN(FRAC_1_SQRT_2)),
    "Math.SQRT2" => Num(JN(SQRT_2)),
    "NaN" => Num(JN(f64::NAN)),
    "Number.EPSILON" => Num(JN(f64::EPSILON)),
    "Number.MAX_SAFE_INTEGER" => Num(JN((2u64.pow(53) - 1) as f64)),
    "Number.MAX_VALUE" => Num(JN(f64::MAX)),
    "Number.MIN_SAFE_INTEGER" => Num(JN(-(2i64.pow(53) - 1) as f64)),
    // Smallest positive subnormal.
    "Number.MIN_VALUE" => Num(JN(f64::from_bits(1))),
    "Number.NaN" => Num(JN(f64::NAN)),
    "Number.NEGATIVE_INFINITY" => Num(JN(f64::NEG_INFINITY)),
    "Number.POSITIVE_INFINITY" => Num(JN(f64::INFINITY)),
    "undefined" => Undefined,
    _ => return None,
  };
  Some(v)
}
