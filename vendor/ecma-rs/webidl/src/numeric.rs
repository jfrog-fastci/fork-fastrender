//! WebIDL numeric conversion helpers.
//!
//! WebIDL numeric conversions are defined in terms of ECMAScript `Number` values. The algorithms in
//! this module operate on an already-`ToNumber`'d `f64` (i.e. they do **not** run `ToNumber`
//! themselves).
//!
//! These helpers are runtime-agnostic and return a lightweight error describing whether the failure
//! should be surfaced as a WebIDL `TypeError` or `RangeError`. Embeddings are responsible for
//! mapping these errors onto their JS engine's throw/error surface.

/// Integer conversion attributes for WebIDL numeric conversions.
///
/// Spec: <https://webidl.spec.whatwg.org/#es-integer-types>
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IntegerConversionAttrs {
  pub clamp: bool,
  pub enforce_range: bool,
}

impl IntegerConversionAttrs {
  #[inline]
  pub const fn is_empty(self) -> bool {
    !self.clamp && !self.enforce_range
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NumericConversionErrorKind {
  TypeError,
  RangeError,
}

/// Error returned by numeric conversions when WebIDL requires throwing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NumericConversionError {
  kind: NumericConversionErrorKind,
  message: &'static str,
}

impl NumericConversionError {
  #[inline]
  pub const fn kind(self) -> NumericConversionErrorKind {
    self.kind
  }

  #[inline]
  pub const fn message(self) -> &'static str {
    self.message
  }

  #[inline]
  pub const fn type_error(message: &'static str) -> Self {
    Self {
      kind: NumericConversionErrorKind::TypeError,
      message,
    }
  }

  #[inline]
  pub const fn range_error(message: &'static str) -> Self {
    Self {
      kind: NumericConversionErrorKind::RangeError,
      message,
    }
  }
}

/// WebIDL abstract operation `convert to int`.
///
/// This implements the shared logic used by `byte`, `octet`, `short`, `unsigned short`, `long`,
/// `unsigned long`, `long long`, and `unsigned long long` conversions.
///
/// Spec: <https://webidl.spec.whatwg.org/#abstract-opdef-converttoint>
pub fn convert_to_int(
  n: f64,
  bit_length: u32,
  signed: bool,
  attrs: IntegerConversionAttrs,
) -> Result<i128, NumericConversionError> {
  debug_assert!((1..=64).contains(&bit_length));

  if attrs.clamp && attrs.enforce_range {
    return Err(NumericConversionError::type_error(
      "[Clamp] and [EnforceRange] cannot both apply to the same type",
    ));
  }

  let (lower_bound, upper_bound): (i128, i128) = if signed {
    let lower_bound = -(1i128 << (bit_length - 1));
    let upper_bound = (1i128 << (bit_length - 1)) - 1;
    (lower_bound, upper_bound)
  } else {
    let upper_bound = (1i128 << bit_length) - 1;
    (0, upper_bound)
  };

  // `ToNumber(V)` is done by the caller; normalize -0 to +0.
  let mut x = n;
  if x == 0.0 && x.is_sign_negative() {
    x = 0.0;
  }

  if attrs.enforce_range {
    if x.is_nan() || x.is_infinite() {
      return Err(NumericConversionError::range_error(
        "EnforceRange integer conversion cannot be NaN/Infinity",
      ));
    }
    let x_int = integer_part(x) as i128;
    if x_int < lower_bound || x_int > upper_bound {
      return Err(NumericConversionError::range_error(
        "integer value is outside EnforceRange bounds",
      ));
    }
    return Ok(x_int);
  }

  if attrs.clamp {
    if x.is_nan() {
      return Ok(0);
    }
    if x.is_infinite() {
      return Ok(if x.is_sign_negative() {
        lower_bound
      } else {
        upper_bound
      });
    }
    let mut y = round_ties_even(x);
    if y == 0.0 && y.is_sign_negative() {
      y = 0.0;
    }
    let y = y as i128;
    return Ok(y.clamp(lower_bound, upper_bound));
  }

  // Default conversion (wrap).
  if x.is_nan() || x == 0.0 || x.is_infinite() {
    return Ok(0);
  }

  let modulo = 1u128 << bit_length;
  let threshold = 1u128 << (bit_length - 1);
  let r = integer_part_modulo_pow2(x, bit_length);

  if signed && r >= threshold {
    Ok(r as i128 - modulo as i128)
  } else {
    Ok(r as i128)
  }
}

/// Convert a `Number` (f64) to a WebIDL `float`.
///
/// Spec: <https://webidl.spec.whatwg.org/#es-float>
pub fn convert_to_float(n: f64) -> Result<f32, NumericConversionError> {
  if n.is_nan() || n.is_infinite() {
    return Err(NumericConversionError::type_error(
      "float must be a finite number",
    ));
  }
  let mut y = n as f32;
  if y.is_infinite() {
    return Err(NumericConversionError::type_error("float is out of range"));
  }
  if y == 0.0 && n.is_sign_negative() {
    y = -0.0;
  }
  Ok(y)
}

/// Convert a `Number` (f64) to a WebIDL `unrestricted float`.
///
/// Spec: <https://webidl.spec.whatwg.org/#es-unrestricted-float>
pub fn convert_to_unrestricted_float(n: f64) -> f32 {
  if n.is_nan() {
    return f32::from_bits(0x7fc0_0000);
  }
  let mut y = n as f32;
  if y == 0.0 && n.is_sign_negative() {
    y = -0.0;
  }
  y
}

/// Convert a `Number` (f64) to a WebIDL `double`.
///
/// Spec: <https://webidl.spec.whatwg.org/#es-double>
pub fn convert_to_double(n: f64) -> Result<f64, NumericConversionError> {
  if n.is_nan() || n.is_infinite() {
    return Err(NumericConversionError::type_error(
      "double must be a finite number",
    ));
  }
  Ok(n)
}

/// Convert a `Number` (f64) to a WebIDL `unrestricted double`.
///
/// Spec: <https://webidl.spec.whatwg.org/#es-unrestricted-double>
pub fn convert_to_unrestricted_double(n: f64) -> f64 {
  if n.is_nan() {
    return f64::from_bits(0x7ff8_0000_0000_0000);
  }
  n
}

fn integer_part(n: f64) -> f64 {
  let r = n.abs().floor();
  if n < 0.0 { -r } else { r }
}

fn round_ties_even(n: f64) -> f64 {
  let floor = n.floor();
  let frac = n - floor;
  if frac < 0.5 {
    return floor;
  }
  if frac > 0.5 {
    return floor + 1.0;
  }
  let floor_int = floor as i64;
  if floor_int % 2 == 0 { floor } else { floor + 1.0 }
}

fn integer_part_modulo_pow2(n: f64, bit_length: u32) -> u128 {
  debug_assert!((1..=64).contains(&bit_length));

  if n == 0.0 {
    // Covers `-0.0` too.
    return 0;
  }

  let bits = n.to_bits();
  let sign = (bits >> 63) != 0;
  let exp_bits = ((bits >> 52) & 0x7ff) as i32;
  let frac_bits = bits & 0x000f_ffff_ffff_ffff;

  // Subnormals (exp_bits == 0) and values with |n| < 1 (exp_unbiased < 0) truncate to 0.
  // The wrap conversion handles NaN/Infinity before calling into this helper.
  if exp_bits == 0 || exp_bits == 0x7ff {
    return 0;
  }

  let exp_unbiased = exp_bits - 1023;
  if exp_unbiased < 0 {
    return 0;
  }

  // 53-bit significand with implicit leading 1.
  let sig = ((1u64 << 52) | frac_bits) as u128;
  let mask = (1u128 << bit_length) - 1;

  // |n| = sig * 2^(exp_unbiased - 52)
  let shift = exp_unbiased - 52;
  let abs_rem = if shift >= 0 {
    let shift = shift as u32;
    if shift >= bit_length {
      0
    } else {
      (sig << shift) & mask
    }
  } else {
    let rshift = (-shift) as u32;
    (sig >> rshift) & mask
  };

  if !sign {
    return abs_rem;
  }
  if abs_rem == 0 {
    return 0;
  }
  (1u128 << bit_length) - abs_rem
}

