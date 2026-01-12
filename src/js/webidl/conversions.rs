//! WebIDL conversions used by generated bindings.
//!
//! This module intentionally depends only on [`super::WebIdlBindingsRuntime`] so the generated
//! bindings can share conversion logic across the real `vm-js` realm runtime and the legacy
//! heap-only runtime.

use std::collections::HashMap;

use super::WebIdlBindingsRuntime;
use crate::js::bindings::BindingValue;

pub use webidl::IntegerConversionAttrs;

fn numeric_conversion_error_to_js<Host, R: WebIdlBindingsRuntime<Host>>(
  rt: &mut R,
  err: webidl::NumericConversionError,
) -> R::Error {
  match err.kind() {
    webidl::NumericConversionErrorKind::TypeError => rt.throw_type_error(err.message()),
    webidl::NumericConversionErrorKind::RangeError => rt.throw_range_error(err.message()),
  }
}

/// Convert an ECMAScript value to a WebIDL enum value.
///
/// Spec: <https://webidl.spec.whatwg.org/#js-to-enumeration>
pub fn to_enum<Host, R>(
  rt: &mut R,
  host: &mut Host,
  value: R::JsValue,
  enum_name: &str,
  allowed_values: &[&str],
) -> Result<String, R::Error>
where
  R: WebIdlBindingsRuntime<Host>,
{
  // Avoid nested mutable borrows of `rt` by splitting `ToString` + `js_string_to_rust_string`
  // into two distinct steps.
  let s = rt.to_string(host, value)?;
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
/// `BindingValue::Record(Vec<(String, BindingValue)>)`.
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
  rt.with_stack_roots(&[value], |rt| {
    let obj = rt.to_object(value)?;
    rt.with_stack_roots(&[obj], |rt| {
      let keys = rt.own_property_keys(obj)?;

      // Root string keys returned by `OwnPropertyKeys` for the duration of the conversion.
      //
      // `vm-js` can synthesize index keys (e.g. for String objects). Those strings are not
      // reachable from `obj` and would be collected unless they are treated as stack roots while
      // we iterate.
      //
      // Note: this intentionally skips Symbol keys here (the WebIDL record algorithm only
      // performs `PropertyKeyToString` after confirming the property is enumerable, so
      // non-enumerable symbol properties should not throw).
      let mut key_roots: Vec<R::JsValue> = Vec::with_capacity(keys.len());
      for key in &keys {
        if rt.property_key_is_symbol(*key) {
          continue;
        }
        key_roots.push(rt.property_key_to_js_string(*key)?);
      }

      let mut entries: Vec<(String, BindingValue<R::JsValue>)> = Vec::new();
      let mut index_by_key: HashMap<String, usize> = HashMap::new();

      rt.with_stack_roots(&key_roots, |rt| {
        for key in keys {
          let Some(desc) = rt.get_own_property(obj, key)? else {
            continue;
          };
          if !desc.enumerable {
            continue;
          }

          // WebIDL record conversion uses `PropertyKeyToString` / `ToString` on property keys:
          // attempting to convert a Symbol key must throw a TypeError. (Non-enumerable properties
          // have already been skipped above.)
          let js_key = rt.property_key_to_js_string(key)?;
          let typed_key = rt.js_string_to_rust_string(js_key)?;

          // Enforce the record entry count limit on *new* keys.
          if !index_by_key.contains_key(&typed_key)
            && entries.len() >= rt.limits().max_record_entries
          {
            return Err(rt.throw_range_error("record exceeds maximum entry count"));
          }

          let typed_value = rt.with_stack_roots(&[js_key], |rt| {
            let prop_value = rt.get(host, obj, key)?;
            rt.with_stack_roots(&[prop_value], |rt| convert_value(rt, host, prop_value))
          })?;
          if let Some(idx) = index_by_key.get(&typed_key).copied() {
            entries[idx].1 = typed_value;
          } else {
            index_by_key.insert(typed_key.clone(), entries.len());
            entries.push((typed_key, typed_value));
          }
        }

        Ok(BindingValue::Record(entries))
      })
    })
  })
}

/// Convert an ECMAScript value to an IDL `byte`.
///
/// Spec: <https://webidl.spec.whatwg.org/#es-byte>
pub fn to_byte<Host, R>(
  rt: &mut R,
  host: &mut Host,
  value: R::JsValue,
  attrs: IntegerConversionAttrs,
) -> Result<i8, R::Error>
where
  R: WebIdlBindingsRuntime<Host>,
{
  let n = rt.to_number(host, value)?;
  let v =
    webidl::convert_to_int(n, 8, true, attrs).map_err(|e| numeric_conversion_error_to_js(rt, e))?;
  Ok(v as i8)
}

/// Convert an ECMAScript value to an IDL `octet`.
///
/// Spec: <https://webidl.spec.whatwg.org/#es-octet>
pub fn to_octet<Host, R>(
  rt: &mut R,
  host: &mut Host,
  value: R::JsValue,
  attrs: IntegerConversionAttrs,
) -> Result<u8, R::Error>
where
  R: WebIdlBindingsRuntime<Host>,
{
  let n = rt.to_number(host, value)?;
  let v = webidl::convert_to_int(n, 8, false, attrs)
    .map_err(|e| numeric_conversion_error_to_js(rt, e))?;
  Ok(v as u8)
}

/// Convert an ECMAScript value to an IDL `short`.
///
/// Spec: <https://webidl.spec.whatwg.org/#es-short>
pub fn to_short<Host, R>(
  rt: &mut R,
  host: &mut Host,
  value: R::JsValue,
  attrs: IntegerConversionAttrs,
) -> Result<i16, R::Error>
where
  R: WebIdlBindingsRuntime<Host>,
{
  let n = rt.to_number(host, value)?;
  let v = webidl::convert_to_int(n, 16, true, attrs)
    .map_err(|e| numeric_conversion_error_to_js(rt, e))?;
  Ok(v as i16)
}

/// Convert an ECMAScript value to an IDL `unsigned short`.
///
/// Spec: <https://webidl.spec.whatwg.org/#es-unsigned-short>
pub fn to_unsigned_short<Host, R>(
  rt: &mut R,
  host: &mut Host,
  value: R::JsValue,
  attrs: IntegerConversionAttrs,
) -> Result<u16, R::Error>
where
  R: WebIdlBindingsRuntime<Host>,
{
  let n = rt.to_number(host, value)?;
  let v = webidl::convert_to_int(n, 16, false, attrs)
    .map_err(|e| numeric_conversion_error_to_js(rt, e))?;
  Ok(v as u16)
}

/// Convert an ECMAScript value to an IDL `long`.
///
/// Spec: <https://webidl.spec.whatwg.org/#es-long>
pub fn to_long<Host, R>(
  rt: &mut R,
  host: &mut Host,
  value: R::JsValue,
  attrs: IntegerConversionAttrs,
) -> Result<i32, R::Error>
where
  R: WebIdlBindingsRuntime<Host>,
{
  let n = rt.to_number(host, value)?;
  let v = webidl::convert_to_int(n, 32, true, attrs)
    .map_err(|e| numeric_conversion_error_to_js(rt, e))?;
  Ok(v as i32)
}

/// Convert an ECMAScript value to an IDL `unsigned long`.
///
/// Spec: <https://webidl.spec.whatwg.org/#es-unsigned-long>
pub fn to_unsigned_long<Host, R>(
  rt: &mut R,
  host: &mut Host,
  value: R::JsValue,
  attrs: IntegerConversionAttrs,
) -> Result<u32, R::Error>
where
  R: WebIdlBindingsRuntime<Host>,
{
  let n = rt.to_number(host, value)?;
  let v = webidl::convert_to_int(n, 32, false, attrs)
    .map_err(|e| numeric_conversion_error_to_js(rt, e))?;
  Ok(v as u32)
}

/// Convert an ECMAScript value to an IDL `long long`.
///
/// Spec: <https://webidl.spec.whatwg.org/#es-long-long>
pub fn to_long_long<Host, R>(
  rt: &mut R,
  host: &mut Host,
  value: R::JsValue,
  attrs: IntegerConversionAttrs,
) -> Result<i64, R::Error>
where
  R: WebIdlBindingsRuntime<Host>,
{
  let n = rt.to_number(host, value)?;
  let v = webidl::convert_to_int(n, 64, true, attrs)
    .map_err(|e| numeric_conversion_error_to_js(rt, e))?;
  Ok(v as i64)
}

/// Convert an ECMAScript value to an IDL `unsigned long long`.
///
/// Spec: <https://webidl.spec.whatwg.org/#es-unsigned-long-long>
pub fn to_unsigned_long_long<Host, R>(
  rt: &mut R,
  host: &mut Host,
  value: R::JsValue,
  attrs: IntegerConversionAttrs,
) -> Result<u64, R::Error>
where
  R: WebIdlBindingsRuntime<Host>,
{
  let n = rt.to_number(host, value)?;
  let v = webidl::convert_to_int(n, 64, false, attrs)
    .map_err(|e| numeric_conversion_error_to_js(rt, e))?;
  Ok(v as u64)
}

/// Convert an ECMAScript value to an IDL `float`.
///
/// Spec: <https://webidl.spec.whatwg.org/#es-float>
pub fn to_float<Host, R>(rt: &mut R, host: &mut Host, value: R::JsValue) -> Result<f32, R::Error>
where
  R: WebIdlBindingsRuntime<Host>,
{
  let x = rt.to_number(host, value)?;
  webidl::convert_to_float(x).map_err(|e| numeric_conversion_error_to_js(rt, e))
}

/// Convert an ECMAScript value to an IDL `unrestricted float`.
///
/// Spec: <https://webidl.spec.whatwg.org/#es-unrestricted-float>
pub fn to_unrestricted_float<Host, R>(
  rt: &mut R,
  host: &mut Host,
  value: R::JsValue,
) -> Result<f32, R::Error>
where
  R: WebIdlBindingsRuntime<Host>,
{
  let x = rt.to_number(host, value)?;
  Ok(webidl::convert_to_unrestricted_float(x))
}

/// Convert an ECMAScript value to an IDL `double`.
///
/// Spec: <https://webidl.spec.whatwg.org/#es-double>
pub fn to_double<Host, R>(rt: &mut R, host: &mut Host, value: R::JsValue) -> Result<f64, R::Error>
where
  R: WebIdlBindingsRuntime<Host>,
{
  let x = rt.to_number(host, value)?;
  webidl::convert_to_double(x).map_err(|e| numeric_conversion_error_to_js(rt, e))
}

/// Convert an ECMAScript value to an IDL `unrestricted double`.
///
/// Spec: <https://webidl.spec.whatwg.org/#es-unrestricted-double>
pub fn to_unrestricted_double<Host, R>(
  rt: &mut R,
  host: &mut Host,
  value: R::JsValue,
) -> Result<f64, R::Error>
where
  R: WebIdlBindingsRuntime<Host>,
{
  let x = rt.to_number(host, value)?;
  Ok(webidl::convert_to_unrestricted_double(x))
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
pub fn to_callback_interface<Host, R>(rt: &mut R, value: R::JsValue) -> Result<R::JsValue, R::Error>
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
    let mut host = ();

    let enum_name = "TestEnum";

    let value = rt.alloc_string_value("a").unwrap();
    let s = to_enum::<(), _>(&mut rt, &mut host, value, enum_name, &["a", "b"]).unwrap();
    assert_eq!(s, "a");

    let invalid = rt.alloc_string_value("c").unwrap();
    let err = to_enum::<(), _>(&mut rt, &mut host, invalid, enum_name, &["a", "b"]).unwrap_err();
    let Some(thrown) = err.thrown_value() else {
      panic!("expected thrown error, got {err:?}");
    };
    let s = webidl_js_runtime::JsRuntime::to_string(&mut rt, thrown).unwrap();
    let msg = as_utf8_lossy(&rt, s);
    assert_eq!(
      msg,
      format!("TypeError: Value is not a valid member of the `{enum_name}` enum")
    );

    // Mirror JS-visible `TypeError#message` (no "TypeError:" prefix).
    let message_key = rt.property_key_from_str("message").unwrap();
    let message_val = webidl_js_runtime::JsRuntime::get(&mut rt, thrown, message_key).unwrap();
    let s = webidl_js_runtime::JsRuntime::to_string(&mut rt, message_val).unwrap();
    let msg = as_utf8_lossy(&rt, s);
    assert_eq!(
      msg,
      format!("Value is not a valid member of the `{enum_name}` enum")
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

    let BindingValue::Record(entries) = record else {
      panic!("expected record, got: {record:?}");
    };

    assert_eq!(entries.len(), 1);
    match &entries[0] {
      (k, BindingValue::String(v)) => {
        assert_eq!(k, "a", "record must include enumerable properties");
        assert_eq!(v, "1", "record must include enumerable properties");
      }
      other => panic!("expected record[0] to be (\"a\", string), got {other:?}"),
    }
  }

  #[test]
  fn record_conversion_throws_type_error_on_enumerable_symbol_keys() {
    let mut rt = VmJsRuntime::new();
    let mut host = ();

    let obj = rt.alloc_object_value().unwrap();
    let a_key = rt.property_key_from_str("a").unwrap();
    webidl_js_runtime::JsRuntime::define_data_property(
      &mut rt,
      obj,
      a_key,
      Value::Number(1.0),
      true,
    )
    .unwrap();

    // WebIDL record conversion uses `PropertyKeyToString` / `ToString` on enumerable keys,
    // so enumerable symbol keys must throw a TypeError.
    let sym_key =
      <VmJsRuntime as webidl_js_runtime::WebIdlJsRuntime>::symbol_iterator(&mut rt).unwrap();
    webidl_js_runtime::JsRuntime::define_data_property(
      &mut rt,
      obj,
      sym_key,
      Value::Number(2.0),
      true,
    )
    .unwrap();

    let err = to_record::<(), _, _>(&mut rt, &mut host, obj, |rt, _host, v| {
      Ok(BindingValue::Number(
        webidl_js_runtime::JsRuntime::to_number(rt, v)?,
      ))
    })
    .unwrap_err();

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
  fn record_conversion_uses_to_object() {
    let mut rt = VmJsRuntime::new();
    let mut host = ();

    // WebIDL record conversion performs `ToObject`, so primitives should be accepted.
    let record = to_record::<(), _, _>(&mut rt, &mut host, Value::Bool(true), |_rt, _host, _v| {
      Ok(BindingValue::Undefined)
    })
    .unwrap();

    let BindingValue::Record(entries) = record else {
      panic!("expected record, got: {record:?}");
    };
    assert!(entries.is_empty());
  }

  #[test]
  fn clamp_unsigned_long_clamps_and_rounds_ties_to_even() {
    let mut rt = VmJsRuntime::new();
    let mut host = ();
    let attrs = IntegerConversionAttrs {
      clamp: true,
      enforce_range: false,
    };

    // Negative values clamp to 0.
    assert_eq!(
      to_unsigned_long::<(), _>(&mut rt, &mut host, Value::Number(-5.0), attrs).unwrap(),
      0
    );

    // Values above 2^32-1 clamp to the upper bound.
    let too_large = (u32::MAX as f64) + 1000.0;
    assert_eq!(
      to_unsigned_long::<(), _>(&mut rt, &mut host, Value::Number(too_large), attrs).unwrap(),
      u32::MAX
    );

    // Rounds ties to even (banker's rounding): 2.5 -> 2, not 3.
    assert_eq!(
      to_unsigned_long::<(), _>(&mut rt, &mut host, Value::Number(2.5), attrs).unwrap(),
      2
    );
  }

  #[test]
  fn enforce_range_long_rejects_nan_infinity_and_out_of_range() {
    let mut rt = VmJsRuntime::new();
    let mut host = ();
    let attrs = IntegerConversionAttrs {
      clamp: false,
      enforce_range: true,
    };

    let err = to_long::<(), _>(&mut rt, &mut host, Value::Number(f64::NAN), attrs).unwrap_err();
    assert_range_error(&mut rt, err);
    let err =
      to_long::<(), _>(&mut rt, &mut host, Value::Number(f64::INFINITY), attrs).unwrap_err();
    assert_range_error(&mut rt, err);
    let err = to_long::<(), _>(
      &mut rt,
      &mut host,
      Value::Number((i32::MAX as f64) + 1.0),
      attrs,
    )
    .unwrap_err();
    assert_range_error(&mut rt, err);
  }

  #[test]
  fn byte_default_integer_conversion_wraps() {
    let mut rt = VmJsRuntime::new();
    let mut host = ();
    let attrs = IntegerConversionAttrs::default();

    // Wrap modulo 256 and then interpret as signed.
    assert_eq!(
      to_byte::<(), _>(&mut rt, &mut host, Value::Number(200.0), attrs).unwrap(),
      -56
    );
    assert_eq!(
      to_byte::<(), _>(&mut rt, &mut host, Value::Number(128.0), attrs).unwrap(),
      -128
    );
    assert_eq!(
      to_byte::<(), _>(&mut rt, &mut host, Value::Number(-129.0), attrs).unwrap(),
      127
    );

    // NaN converts to 0.
    assert_eq!(
      to_byte::<(), _>(&mut rt, &mut host, Value::Number(f64::NAN), attrs).unwrap(),
      0
    );
  }

  #[test]
  fn long_long_default_integer_conversion_wraps() {
    let mut rt = VmJsRuntime::new();
    let mut host = ();
    let attrs = IntegerConversionAttrs::default();

    assert_eq!(
      to_long_long::<(), _>(&mut rt, &mut host, Value::Number(-1.0), attrs).unwrap(),
      -1
    );
    assert_eq!(
      to_unsigned_long_long::<(), _>(&mut rt, &mut host, Value::Number(-1.0), attrs).unwrap(),
      u64::MAX
    );
  }
}
