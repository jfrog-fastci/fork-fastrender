//! WebIDL conversions used by generated bindings.
//!
//! This module intentionally depends only on [`super::WebIdlBindingsRuntime`] so the generated
//! bindings can share conversion logic across the real `vm-js` realm runtime and the legacy
//! heap-only runtime.

use std::collections::BTreeMap;

use crate::js::bindings::BindingValue;
use super::WebIdlBindingsRuntime;

#[derive(Debug, Clone, Copy, Default)]
pub struct IntegerConversionAttrs {
  pub clamp: bool,
  pub enforce_range: bool,
}

/// Convert an ECMAScript value to a WebIDL enum value.
///
/// Spec: <https://webidl.spec.whatwg.org/#js-to-enumeration>
pub fn to_enum<Host, R>(
  rt: &mut R,
  value: R::JsValue,
  enum_name: &str,
  allowed_values: &[&str],
) -> Result<String, R::Error>
where
  R: WebIdlBindingsRuntime<Host>,
{
  // Avoid nested mutable borrows of `rt` by splitting `ToString` + `js_string_to_rust_string`
  // into two distinct steps.
  let s = rt.to_string(value)?;
  let s = rt.js_string_to_rust_string(s)?;
  if !allowed_values.iter().any(|v| *v == s) {
    return Err(rt.throw_type_error(&format!(
      "Value is not a valid member of the `{enum_name}` enum"
    )));
  }
  Ok(s)
}

/// Convert an ECMAScript value to a WebIDL `record<K, V>`.
///
/// This returns the binding-layer representation:
/// `BindingValue::Dictionary(BTreeMap<_, _>)`.
///
/// Spec: <https://webidl.spec.whatwg.org/#js-to-record>
pub fn to_record<Host, R, F>(
  rt: &mut R,
  host: &mut Host,
  value: R::JsValue,
  mut convert_value: F,
) -> Result<BindingValue<R::JsValue>, R::Error>
where
  R: WebIdlBindingsRuntime<Host>,
  F: FnMut(&mut R, &mut Host, R::JsValue) -> Result<BindingValue<R::JsValue>, R::Error>,
{
  if !rt.is_object(value) {
    return Err(rt.throw_type_error("expected object for record"));
  }

  rt.with_stack_roots(&[value], |rt| {
    let keys = rt.own_property_keys(value)?;
    let mut out: BTreeMap<String, BindingValue<R::JsValue>> = BTreeMap::new();

    for key in keys {
      let Some(desc) = rt.get_own_property(value, key)? else {
        continue;
      };
      if !desc.enumerable {
        continue;
      }

      let js_key = rt.property_key_to_js_string(key)?;
      let typed_key = rt.js_string_to_rust_string(js_key)?;

      // Enforce the record entry count limit on *new* keys.
      if !out.contains_key(&typed_key) && out.len() >= rt.limits().max_record_entries {
        return Err(rt.throw_range_error("record exceeds maximum entry count"));
      }

      // Root the key while fetching and converting the property value. `own_property_keys` can
      // synthesize index keys (e.g. for String objects), which are not reachable from `value` and
      // must be treated as stack roots during the conversion.
      let typed_value = rt.with_stack_roots(&[js_key], |rt| {
        let prop_value = rt.get(value, key)?;
        rt.with_stack_roots(&[prop_value], |rt| convert_value(rt, host, prop_value))
      })?;
      out.insert(typed_key, typed_value);
    }

    Ok(BindingValue::Dictionary(out))
  })
}

/// Convert an ECMAScript value to an IDL `byte`.
///
/// Spec: <https://webidl.spec.whatwg.org/#es-byte>
pub fn to_byte<Host, R>(
  rt: &mut R,
  value: R::JsValue,
  attrs: IntegerConversionAttrs,
) -> Result<i8, R::Error>
where
  R: WebIdlBindingsRuntime<Host>,
{
  let n = rt.to_number(value)?;
  let v = convert_to_int(rt, n, 8, true, attrs)?;
  Ok(v as i8)
}

/// Convert an ECMAScript value to an IDL `octet`.
///
/// Spec: <https://webidl.spec.whatwg.org/#es-octet>
pub fn to_octet<Host, R>(
  rt: &mut R,
  value: R::JsValue,
  attrs: IntegerConversionAttrs,
) -> Result<u8, R::Error>
where
  R: WebIdlBindingsRuntime<Host>,
{
  let n = rt.to_number(value)?;
  let v = convert_to_int(rt, n, 8, false, attrs)?;
  Ok(v as u8)
}

/// Convert an ECMAScript value to an IDL `short`.
///
/// Spec: <https://webidl.spec.whatwg.org/#es-short>
pub fn to_short<Host, R>(
  rt: &mut R,
  value: R::JsValue,
  attrs: IntegerConversionAttrs,
) -> Result<i16, R::Error>
where
  R: WebIdlBindingsRuntime<Host>,
{
  let n = rt.to_number(value)?;
  let v = convert_to_int(rt, n, 16, true, attrs)?;
  Ok(v as i16)
}

/// Convert an ECMAScript value to an IDL `unsigned short`.
///
/// Spec: <https://webidl.spec.whatwg.org/#es-unsigned-short>
pub fn to_unsigned_short<Host, R>(
  rt: &mut R,
  value: R::JsValue,
  attrs: IntegerConversionAttrs,
) -> Result<u16, R::Error>
where
  R: WebIdlBindingsRuntime<Host>,
{
  let n = rt.to_number(value)?;
  let v = convert_to_int(rt, n, 16, false, attrs)?;
  Ok(v as u16)
}

/// Convert an ECMAScript value to an IDL `long`.
///
/// Spec: <https://webidl.spec.whatwg.org/#es-long>
pub fn to_long<Host, R>(
  rt: &mut R,
  value: R::JsValue,
  attrs: IntegerConversionAttrs,
) -> Result<i32, R::Error>
where
  R: WebIdlBindingsRuntime<Host>,
{
  let n = rt.to_number(value)?;
  let v = convert_to_int(rt, n, 32, true, attrs)?;
  Ok(v as i32)
}

/// Convert an ECMAScript value to an IDL `unsigned long`.
///
/// Spec: <https://webidl.spec.whatwg.org/#es-unsigned-long>
pub fn to_unsigned_long<Host, R>(
  rt: &mut R,
  value: R::JsValue,
  attrs: IntegerConversionAttrs,
) -> Result<u32, R::Error>
where
  R: WebIdlBindingsRuntime<Host>,
{
  let n = rt.to_number(value)?;
  let v = convert_to_int(rt, n, 32, false, attrs)?;
  Ok(v as u32)
}

/// Convert an ECMAScript value to an IDL `long long`.
///
/// Spec: <https://webidl.spec.whatwg.org/#es-long-long>
pub fn to_long_long<Host, R>(
  rt: &mut R,
  value: R::JsValue,
  attrs: IntegerConversionAttrs,
) -> Result<i64, R::Error>
where
  R: WebIdlBindingsRuntime<Host>,
{
  let n = rt.to_number(value)?;
  let v = convert_to_int(rt, n, 64, true, attrs)?;
  Ok(v as i64)
}

/// Convert an ECMAScript value to an IDL `unsigned long long`.
///
/// Spec: <https://webidl.spec.whatwg.org/#es-unsigned-long-long>
pub fn to_unsigned_long_long<Host, R>(
  rt: &mut R,
  value: R::JsValue,
  attrs: IntegerConversionAttrs,
) -> Result<u64, R::Error>
where
  R: WebIdlBindingsRuntime<Host>,
{
  let n = rt.to_number(value)?;
  let v = convert_to_int(rt, n, 64, false, attrs)?;
  Ok(v as u64)
}

/// Convert an ECMAScript value to an IDL `float`.
///
/// Spec: <https://webidl.spec.whatwg.org/#es-float>
pub fn to_float<Host, R>(rt: &mut R, value: R::JsValue) -> Result<f32, R::Error>
where
  R: WebIdlBindingsRuntime<Host>,
{
  let x = rt.to_number(value)?;
  if x.is_nan() || x.is_infinite() {
    return Err(rt.throw_type_error("float must be a finite number"));
  }
  let mut y = x as f32;
  if y.is_infinite() {
    return Err(rt.throw_type_error("float is out of range"));
  }
  if y == 0.0 && x.is_sign_negative() {
    y = -0.0;
  }
  Ok(y)
}

/// Convert an ECMAScript value to an IDL `unrestricted float`.
///
/// Spec: <https://webidl.spec.whatwg.org/#es-unrestricted-float>
pub fn to_unrestricted_float<Host, R>(rt: &mut R, value: R::JsValue) -> Result<f32, R::Error>
where
  R: WebIdlBindingsRuntime<Host>,
{
  let x = rt.to_number(value)?;
  if x.is_nan() {
    return Ok(f32::from_bits(0x7fc0_0000));
  }
  let mut y = x as f32;
  if y == 0.0 && x.is_sign_negative() {
    y = -0.0;
  }
  Ok(y)
}

/// Convert an ECMAScript value to an IDL `double`.
///
/// Spec: <https://webidl.spec.whatwg.org/#es-double>
pub fn to_double<Host, R>(rt: &mut R, value: R::JsValue) -> Result<f64, R::Error>
where
  R: WebIdlBindingsRuntime<Host>,
{
  let x = rt.to_number(value)?;
  if x.is_nan() || x.is_infinite() {
    return Err(rt.throw_type_error("double must be a finite number"));
  }
  Ok(x)
}

/// Convert an ECMAScript value to an IDL `unrestricted double`.
///
/// Spec: <https://webidl.spec.whatwg.org/#es-unrestricted-double>
pub fn to_unrestricted_double<Host, R>(rt: &mut R, value: R::JsValue) -> Result<f64, R::Error>
where
  R: WebIdlBindingsRuntime<Host>,
{
  let x = rt.to_number(value)?;
  if x.is_nan() {
    return Ok(f64::from_bits(0x7ff8_0000_0000_0000));
  }
  Ok(x)
}

fn convert_to_int<Host, R: WebIdlBindingsRuntime<Host>>(
  rt: &mut R,
  n: f64,
  bit_length: u32,
  signed: bool,
  attrs: IntegerConversionAttrs,
) -> Result<i128, R::Error> {
  if attrs.clamp && attrs.enforce_range {
    return Err(rt.throw_type_error(
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
      return Err(rt.throw_range_error(
        "EnforceRange integer conversion cannot be NaN/Infinity",
      ));
    }
    let x_int = integer_part(x) as i128;
    if x_int < lower_bound || x_int > upper_bound {
      return Err(rt.throw_range_error(
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
  let floor_int = floor as i64;
  if floor_int % 2 == 0 {
    floor
  } else {
    floor + 1.0
  }
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

/// Convert an ECMAScript value to a WebIDL callback function value.
///
/// Spec: <https://webidl.spec.whatwg.org/#es-callback-function>
pub fn to_callback_function<Host, R>(
  rt: &mut R,
  value: R::JsValue,
  legacy_treat_non_object_as_null: bool,
) -> Result<R::JsValue, R::Error>
where
  R: WebIdlBindingsRuntime<Host>,
{
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
///
/// MVP behaviour: validate that `value` is an object and return it.
pub fn to_callback_interface<Host, R>(
  rt: &mut R,
  value: R::JsValue,
) -> Result<R::JsValue, R::Error>
where
  R: WebIdlBindingsRuntime<Host>,
{
  if rt.is_object(value) {
    return Ok(value);
  }
  Err(rt.throw_type_error("Value is not a callback interface object"))
}

#[cfg(test)]
mod tests {
  use super::*;
  use vm_js::Value;
  use webidl_js_runtime::JsRuntime as _;
  use webidl_js_runtime::VmJsRuntime;

  fn as_utf8_lossy(rt: &VmJsRuntime, v: Value) -> String {
    let Value::String(s) = v else {
      panic!("expected string");
    };
    rt.heap().get_string(s).unwrap().to_utf8_lossy()
  }

  fn assert_range_error(rt: &mut VmJsRuntime, err: vm_js::VmError) {
    let Some(thrown) = err.thrown_value() else {
      panic!("expected thrown error, got {err:?}");
    };
    let s = webidl_js_runtime::JsRuntime::to_string(rt, thrown).unwrap();
    let msg = as_utf8_lossy(rt, s);
    assert!(
      msg.starts_with("RangeError:"),
      "expected RangeError, got {msg:?}"
    );
  }

  #[test]
  fn enum_conversion_accepts_known_values_and_rejects_invalid() {
    let mut rt = VmJsRuntime::new();

    let value = rt.alloc_string_value("a").unwrap();
    let s = to_enum::<(), _>(&mut rt, value, "E", &["a", "b"]).unwrap();
    assert_eq!(s, "a");

    let invalid = rt.alloc_string_value("c").unwrap();
    let err = to_enum::<(), _>(&mut rt, invalid, "E", &["a", "b"]).unwrap_err();
    let Some(thrown) = err.thrown_value() else {
      panic!("expected thrown error, got {err:?}");
    };
    let s = webidl_js_runtime::JsRuntime::to_string(&mut rt, thrown).unwrap();
    let msg = as_utf8_lossy(&rt, s);
    assert!(
      msg.starts_with("TypeError:"),
      "expected TypeError, got {msg:?}"
    );
  }

  #[test]
  fn record_conversion_collects_own_enumerable_properties() {
    let mut rt = VmJsRuntime::new();
    let mut host = ();

    let obj = rt.alloc_object_value().unwrap();
    let a_key = rt.property_key_from_str("a").unwrap();
    let hidden_key = rt.property_key_from_str("hidden").unwrap();

    let a_val = rt.alloc_string_value("1").unwrap();
    let hidden_val = rt.alloc_string_value("2").unwrap();

    webidl_js_runtime::JsRuntime::define_data_property(&mut rt, obj, a_key, a_val, true).unwrap();
    webidl_js_runtime::JsRuntime::define_data_property(&mut rt, obj, hidden_key, hidden_val, false)
      .unwrap();

    let record = to_record::<(), _, _>(&mut rt, &mut host, obj, |rt, _host, v| {
      let s = webidl_js_runtime::JsRuntime::to_string(rt, v)?;
      Ok(BindingValue::String(rt.string_to_utf8_lossy(s)?))
    })
    .unwrap();

    let BindingValue::Dictionary(map) = record else {
      panic!("expected dictionary record, got: {record:?}");
    };

    assert_eq!(map.len(), 1);
    match map.get("a") {
      Some(BindingValue::String(v)) => assert_eq!(v, "1", "record must include enumerable properties"),
      other => panic!("expected record['a'] to be a string, got {other:?}"),
    }
    assert!(!map.contains_key("hidden"), "record must skip non-enumerable keys");
  }

  #[test]
  fn clamp_unsigned_long_clamps_and_rounds_ties_to_even() {
    let mut rt = VmJsRuntime::new();
    let attrs = IntegerConversionAttrs {
      clamp: true,
      enforce_range: false,
    };

    // Negative values clamp to 0.
    assert_eq!(
      to_unsigned_long::<(), _>(&mut rt, Value::Number(-5.0), attrs).unwrap(),
      0
    );

    // Values above 2^32-1 clamp to the upper bound.
    let too_large = (u32::MAX as f64) + 1000.0;
    assert_eq!(
      to_unsigned_long::<(), _>(&mut rt, Value::Number(too_large), attrs).unwrap(),
      u32::MAX
    );

    // Rounds ties to even (banker's rounding): 2.5 -> 2, not 3.
    assert_eq!(
      to_unsigned_long::<(), _>(&mut rt, Value::Number(2.5), attrs).unwrap(),
      2
    );
  }

  #[test]
  fn enforce_range_long_rejects_nan_infinity_and_out_of_range() {
    let mut rt = VmJsRuntime::new();
    let attrs = IntegerConversionAttrs {
      clamp: false,
      enforce_range: true,
    };

    let err = to_long::<(), _>(&mut rt, Value::Number(f64::NAN), attrs).unwrap_err();
    assert_range_error(&mut rt, err);
    let err = to_long::<(), _>(&mut rt, Value::Number(f64::INFINITY), attrs).unwrap_err();
    assert_range_error(&mut rt, err);
    let err = to_long::<(), _>(&mut rt, Value::Number((i32::MAX as f64) + 1.0), attrs).unwrap_err();
    assert_range_error(&mut rt, err);
  }

  #[test]
  fn byte_default_integer_conversion_wraps() {
    let mut rt = VmJsRuntime::new();
    let attrs = IntegerConversionAttrs::default();

    // Wrap modulo 256 and then interpret as signed.
    assert_eq!(to_byte::<(), _>(&mut rt, Value::Number(200.0), attrs).unwrap(), -56);
    assert_eq!(to_byte::<(), _>(&mut rt, Value::Number(128.0), attrs).unwrap(), -128);
    assert_eq!(to_byte::<(), _>(&mut rt, Value::Number(-129.0), attrs).unwrap(), 127);

    // NaN converts to 0.
    assert_eq!(to_byte::<(), _>(&mut rt, Value::Number(f64::NAN), attrs).unwrap(), 0);
  }

  #[test]
  fn long_long_default_integer_conversion_wraps() {
    let mut rt = VmJsRuntime::new();
    let attrs = IntegerConversionAttrs::default();

    assert_eq!(
      to_long_long::<(), _>(&mut rt, Value::Number(-1.0), attrs).unwrap(),
      -1
    );
    assert_eq!(
      to_unsigned_long_long::<(), _>(&mut rt, Value::Number(-1.0), attrs).unwrap(),
      u64::MAX
    );
  }
}
