//! Web IDL → Rust conversion helpers built on [`WebIdlJsRuntime`].
//!
//! WebIDL conversion algorithms often need to distinguish between `TypeError` and `RangeError`
//! failures. The pure conversion logic in this module returns [`webidl_ir::WebIdlException`], which
//! is then mapped to the embedded engine's throw type via [`WebIdlJsRuntime`].
//!
//! This module also provides callback type conversions (callback functions and callback
//! interfaces). These conversions currently return the underlying engine value so bindings can
//! store and invoke callbacks later.

use crate::{JsRuntime, WebIdlJsRuntime};
use webidl_ir::{IdlType, NamedTypeKind, TypeAnnotation, WebIdlException};

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

fn is_null_or_undefined<R: JsRuntime>(rt: &R, value: R::JsValue) -> bool {
  // ECMAScript values are:
  // `Undefined | Null | Boolean | Number | BigInt | String | Symbol | Object`.
  //
  // WebIDL algorithms frequently treat `undefined` and `null` as a single bucket; we derive
  // "null or undefined" by excluding every other type.
  !rt.is_object(value)
    && !rt.is_boolean(value)
    && !rt.is_number(value)
    && !rt.is_bigint(value)
    && !rt.is_string(value)
    && !rt.is_symbol(value)
}

/// Convert an ECMAScript value to a WebIDL callback function value.
///
/// Spec: <https://webidl.spec.whatwg.org/#es-callback-function>
pub fn to_callback_function<R: WebIdlJsRuntime>(
  rt: &mut R,
  value: R::JsValue,
  legacy_treat_non_object_as_null: bool,
) -> Result<R::JsValue, R::Error> {
  if legacy_treat_non_object_as_null && !rt.is_object(value) {
    return Ok(rt.js_null());
  }
  if rt.is_callable(value) {
    return Ok(value);
  }
  Err(rt.throw_type_error("Value is not a callable callback function"))
}

/// Convert an ECMAScript value to a WebIDL callback interface value.
///
/// Spec: <https://webidl.spec.whatwg.org/#es-callback-interface>
pub fn to_callback_interface<R: WebIdlJsRuntime>(
  rt: &mut R,
  value: R::JsValue,
) -> Result<R::JsValue, R::Error> {
  if rt.is_callable(value) {
    return Ok(value);
  }
  if !rt.is_object(value) {
    return Err(rt.throw_type_error("Value is not a callable callback interface"));
  }

  let handle_event_key = rt.property_key_from_str("handleEvent")?;
  match rt.get_method(value, handle_event_key)? {
    Some(_) => Ok(value),
    None => Err(rt.throw_type_error(
      "Callback interface object is missing a callable handleEvent method",
    )),
  }
}

/// Convert an ECMAScript value to a WebIDL interface type.
///
/// Spec: <https://webidl.spec.whatwg.org/#es-interface>
pub fn to_interface<R: WebIdlJsRuntime>(
  rt: &mut R,
  value: R::JsValue,
  interface: &str,
) -> Result<R::JsValue, R::Error> {
  if rt.implements_interface(value, interface) {
    return Ok(value);
  }
  Err(rt.throw_type_error("Value is not a platform object implementing the expected interface"))
}

fn convert_to_callback_internal<R: WebIdlJsRuntime>(
  rt: &mut R,
  value: R::JsValue,
  ty: &IdlType,
  legacy_treat_non_object_as_null: bool,
) -> Result<R::JsValue, R::Error> {
  match ty {
    IdlType::Annotated { annotations, inner } => {
      let legacy = legacy_treat_non_object_as_null
        || annotations
          .iter()
          .any(|ann| matches!(ann, TypeAnnotation::LegacyTreatNonObjectAsNull));
      convert_to_callback_internal(rt, value, inner, legacy)
    }
    IdlType::Nullable(inner) => {
      if is_null_or_undefined(rt, value) {
        return Ok(rt.js_null());
      }
      convert_to_callback_internal(rt, value, inner, legacy_treat_non_object_as_null)
    }
    IdlType::Named(named) => match named.kind {
      NamedTypeKind::CallbackFunction => {
        to_callback_function(rt, value, legacy_treat_non_object_as_null)
      }
      NamedTypeKind::CallbackInterface => {
        // Callback interfaces are normally structural (`handleEvent`) or callable. However, bindings
        // generators may also treat embedding-defined platform objects as implementing callback
        // interfaces, so accept those first.
        if rt.implements_interface(value, &named.name) {
          return Ok(value);
        }
        to_callback_interface(rt, value)
      }
      _ => Err(rt.throw_type_error("Expected a callback function or callback interface type")),
    },
    _ => Err(rt.throw_type_error("Expected a callback function or callback interface type")),
  }
}

/// Convert an ECMAScript value to a callback type described by `ty`.
///
/// This helper only supports callback functions and callback interfaces (with optional `nullable`
/// and `[LegacyTreatNonObjectAsNull]` wrappers). It returns the underlying engine value (function
/// or object) so the host can store it and invoke it later.
pub fn convert_to_callback<R: WebIdlJsRuntime>(
  rt: &mut R,
  value: R::JsValue,
  ty: &IdlType,
) -> Result<R::JsValue, R::Error> {
  convert_to_callback_internal(rt, value, ty, false)
}

fn convert_to_interface_internal<R: WebIdlJsRuntime>(
  rt: &mut R,
  value: R::JsValue,
  ty: &IdlType,
) -> Result<R::JsValue, R::Error> {
  match ty {
    IdlType::Annotated { inner, .. } => convert_to_interface_internal(rt, value, inner),
    IdlType::Nullable(inner) => {
      if is_null_or_undefined(rt, value) {
        return Ok(rt.js_null());
      }
      convert_to_interface_internal(rt, value, inner)
    }
    IdlType::Named(named) => match named.kind {
      NamedTypeKind::Interface => to_interface(rt, value, &named.name),
      _ => Err(rt.throw_type_error("Expected an interface type")),
    },
    _ => Err(rt.throw_type_error("Expected an interface type")),
  }
}

/// Convert an ECMAScript value to a WebIDL interface type described by `ty`.
///
/// This helper supports `interface` types with optional `nullable` and `Annotated` wrappers. It
/// returns the underlying engine value so bindings can retain the original JS wrapper (and then
/// use [`WebIdlJsRuntime::platform_object_opaque`] to map it back to host data if needed).
pub fn convert_to_interface<R: WebIdlJsRuntime>(
  rt: &mut R,
  value: R::JsValue,
  ty: &IdlType,
) -> Result<R::JsValue, R::Error> {
  convert_to_interface_internal(rt, value, ty)
}

/// Invoke a previously-converted callback interface value.
///
/// - If `callback` is callable, it is called with `this = undefined`.
/// - Otherwise, it is treated as an object and `callback.handleEvent(...args)` is invoked.
pub fn invoke_callback_interface<R: WebIdlJsRuntime>(
  rt: &mut R,
  callback: R::JsValue,
  args: &[R::JsValue],
) -> Result<R::JsValue, R::Error> {
  if rt.is_callable(callback) {
    return rt.call(callback, rt.js_undefined(), args);
  }
  if !rt.is_object(callback) {
    return Err(rt.throw_type_error("Callback interface value is not callable or an object"));
  }

  let handle_event_key = rt.property_key_from_str("handleEvent")?;
  let Some(handle_event) = rt.get_method(callback, handle_event_key)? else {
    return Err(rt.throw_type_error(
      "Callback interface object is missing a callable handleEvent method",
    ));
  };

  rt.call(handle_event, callback, args)
}

#[cfg(test)]
mod tests {
  use super::*;
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
