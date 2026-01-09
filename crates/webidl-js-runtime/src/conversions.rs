//! Web IDL → Rust conversion helpers built on [`WebIdlJsRuntime`].
//!
//! WebIDL conversion algorithms often need to distinguish between `TypeError` and `RangeError`
//! failures. The pure conversion logic in this module returns [`webidl_ir::WebIdlException`], which
//! is then mapped to the embedded engine's throw type via [`WebIdlJsRuntime`].

use crate::WebIdlJsRuntime;
use webidl_ir::WebIdlException;

#[derive(Debug, Clone, Copy, Default)]
pub struct IntegerConversionAttrs {
  pub clamp: bool,
  pub enforce_range: bool,
}

impl IntegerConversionAttrs {
  pub fn is_empty(self) -> bool {
    !self.clamp && !self.enforce_range
  }
}

fn throw_webidl_exception<R: WebIdlJsRuntime>(rt: &mut R, err: WebIdlException) -> R::Error {
  match err {
    WebIdlException::TypeError { message } => rt.throw_type_error(&message),
    WebIdlException::RangeError { message } => rt.throw_range_error(&message),
  }
}

/// Convert an ECMAScript value to an IDL `byte`.
///
/// Spec: <https://webidl.spec.whatwg.org/#es-byte>
pub fn to_byte<R: WebIdlJsRuntime>(
  rt: &mut R,
  value: R::JsValue,
  attrs: IntegerConversionAttrs,
) -> Result<i8, R::Error> {
  let n = rt.to_number(value)?;
  let v = convert_to_int(n, 8, true, attrs).map_err(|e| throw_webidl_exception(rt, e))?;
  Ok(v as i8)
}

fn convert_to_int(
  n: f64,
  bit_length: u32,
  signed: bool,
  ext: IntegerConversionAttrs,
) -> Result<f64, WebIdlException> {
  if !signed && bit_length == 0 {
    return Err(WebIdlException::type_error(
      "integer conversion requires a non-zero bit length",
    ));
  }

  let (lower_bound, upper_bound) = if bit_length == 64 {
    // WebIDL defines `long long`/`unsigned long long` conversion bounds using the "safe integer"
    // range because ECMAScript Numbers cannot precisely represent all 64-bit integers.
    let upper_bound = (1u64 << 53) as f64 - 1.0;
    let lower_bound = if signed {
      -((1u64 << 53) as f64) + 1.0
    } else {
      0.0
    };
    (lower_bound, upper_bound)
  } else if signed {
    let lower_bound = -((1u64 << (bit_length - 1)) as f64);
    let upper_bound = ((1u64 << (bit_length - 1)) as f64) - 1.0;
    (lower_bound, upper_bound)
  } else {
    let lower_bound = 0.0;
    let upper_bound = ((1u64 << bit_length) as f64) - 1.0;
    (lower_bound, upper_bound)
  };

  // `ToNumber(V)` is done by the caller; normalize -0 to +0.
  let mut x = n;
  if x == 0.0 && x.is_sign_negative() {
    x = 0.0;
  }

  if ext.enforce_range {
    if x.is_nan() || x.is_infinite() {
      return Err(WebIdlException::range_error(
        "EnforceRange integer conversion cannot be NaN/Infinity",
      ));
    }
    x = integer_part(x);
    if x < lower_bound || x > upper_bound {
      return Err(WebIdlException::range_error(
        "integer value is outside EnforceRange bounds",
      ));
    }
    return Ok(x);
  }

  if ext.clamp && !x.is_nan() {
    x = x.clamp(lower_bound, upper_bound);
    x = round_ties_even(x);
    if x == 0.0 && x.is_sign_negative() {
      x = 0.0;
    }
    return Ok(x);
  }

  if x.is_nan() || x == 0.0 || x.is_infinite() {
    return Ok(0.0);
  }

  x = integer_part(x);

  let modulo = 2f64.powi(bit_length as i32);
  x = x.rem_euclid(modulo);

  if signed {
    let threshold = 2f64.powi((bit_length - 1) as i32);
    if x >= threshold {
      return Ok(x - modulo);
    }
  }

  Ok(x)
}

fn integer_part(n: f64) -> f64 {
  let r = n.abs().floor();
  if n < 0.0 {
    -r
  } else {
    r
  }
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
  // exactly halfway between two integers
  let floor_int = floor as i64;
  if floor_int % 2 == 0 {
    floor
  } else {
    floor + 1.0
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::JsRuntime;
  use crate::VmJsRuntime;
  use vm_js::{Value, VmError};

  fn as_utf8_lossy(rt: &VmJsRuntime, v: Value) -> String {
    let Value::String(s) = v else {
      panic!("expected string");
    };
    rt.heap().get_string(s).unwrap().to_utf8_lossy()
  }

  #[test]
  fn enforce_range_integer_conversion_throws_range_error() {
    let mut rt = VmJsRuntime::new();

    let err = to_byte(
      &mut rt,
      Value::Number(200.0),
      IntegerConversionAttrs {
        enforce_range: true,
        clamp: false,
      },
    )
    .expect_err("out-of-range enforce-range conversion should throw");

    let VmError::Throw(thrown) = err else {
      panic!("expected VmError::Throw, got {err:?}");
    };

    let s = rt.to_string(thrown).unwrap();
    let msg = as_utf8_lossy(&rt, s);
    assert!(
      msg.starts_with("RangeError:"),
      "expected RangeError, got {msg:?}"
    );
  }
}
