use vm_js::{Value, VmError};
use webidl_js_runtime::{JsRuntime, VmJsRuntime, WebIdlJsRuntime};

#[test]
fn string_property_key_roundtrips_with_get() -> Result<(), VmError> {
  let mut rt = VmJsRuntime::new();
  let obj = rt.alloc_object_value()?;

  let key = rt.property_key_from_str("x")?;

  rt.define_data_property(obj, key, Value::Number(42.0), true)?;
  let got = rt.get(obj, key)?;
  assert_eq!(got, Value::Number(42.0));

  let keys = rt.own_property_keys(obj)?;
  assert_eq!(keys, vec![key]);

  let name = rt.property_key_to_js_string(key)?;
  let units = rt.with_string_code_units(name, |view| view.to_vec())?;
  assert_eq!(units, vec![b'x' as u16]);
  Ok(())
}

#[test]
fn string_code_units_roundtrip_and_support_string_objects() -> Result<(), VmError> {
  let mut rt = VmJsRuntime::new();

  // Include an unpaired surrogate to ensure we preserve raw UTF-16 code units.
  let units = vec![0x0061u16, 0xD800u16, 0x0062u16];
  let s = rt.alloc_string_from_code_units(&units)?;
  assert!(rt.is_string(s));

  let got = rt.with_string_code_units(s, |view| view.to_vec())?;
  assert_eq!(got, units);

  // `with_string_code_units` should also accept String objects.
  let obj = rt.to_object(s)?;
  assert!(rt.is_string_object(obj));
  let got = rt.with_string_code_units(obj, |view| view.to_vec())?;
  assert_eq!(got, units);

  Ok(())
}

#[test]
fn own_property_keys_orders_array_indices_then_strings_then_symbols() -> Result<(), VmError> {
  let mut rt = VmJsRuntime::new();
  let obj = rt.alloc_object()?;

  let key_b = rt.property_key_from_str("b")?;
  let key_2 = rt.property_key_from_u32(2)?;
  let sym_iter = rt.symbol_iterator()?;
  let key_a = rt.property_key_from_str("a")?;
  let key_0 = rt.property_key_from_u32(0)?;
  let sym_async_iter = rt.symbol_async_iterator()?;
  let key_1 = rt.property_key_from_u32(1)?;

  // Insert in a deliberately mixed order so the test can validate the ordering rules.
  rt.define_data_property(obj, key_b, Value::Number(1.0), true)?;
  rt.define_data_property(obj, key_2, Value::Number(2.0), true)?;
  rt.define_data_property(obj, sym_iter, Value::Number(3.0), true)?;
  rt.define_data_property(obj, key_a, Value::Number(4.0), true)?;
  rt.define_data_property(obj, key_0, Value::Number(5.0), true)?;
  rt.define_data_property(obj, sym_async_iter, Value::Number(6.0), true)?;
  rt.define_data_property(obj, key_1, Value::Number(7.0), true)?;

  let keys = rt.own_property_keys(obj)?;
  assert_eq!(
    keys,
    vec![key_0, key_1, key_2, key_b, key_a, sym_iter, sym_async_iter]
  );

  Ok(())
}
