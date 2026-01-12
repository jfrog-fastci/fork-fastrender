use std::rc::Rc;

use vm_js::{HeapLimits, Value};
use webidl::{convert_js_to_idl, IdlType, IdlValue};
use webidl_runtime::{
  JsRuntime as LegacyJsRuntime, VmJsRuntime, WebIdlJsRuntime as LegacyWebIdlJsRuntime,
};

#[test]
fn vm_js_runtime_implements_ecma_webidl_jsruntime_for_sequence_conversion() {
  let mut rt = VmJsRuntime::with_limits(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));

  let iterable = rt.alloc_object_value().unwrap();

  let items: Rc<Vec<Value>> = Rc::new(vec![
    Value::Number(1.0),
    Value::Number(2.0),
    Value::Number(3.0),
  ]);

  let items_for_next = items.clone();
  let next = rt
    .alloc_function_value(move |rt, this, _args| {
      let idx_key = rt.property_key_from_str("index")?;
      let idx_value = rt.get(this, idx_key)?;
      let idx = match idx_value {
        Value::Number(n) => n as usize,
        _ => 0,
      };
      let done = idx >= items_for_next.len();
      let value = if done {
        Value::Undefined
      } else {
        items_for_next[idx]
      };

      rt.define_data_property(this, idx_key, Value::Number((idx + 1) as f64), true)?;

      let result = rt.alloc_object_value()?;
      let done_key = rt.property_key_from_str("done")?;
      let value_key = rt.property_key_from_str("value")?;
      rt.define_data_property(result, done_key, Value::Bool(done), true)?;
      rt.define_data_property(result, value_key, value, true)?;
      Ok(result)
    })
    .unwrap();

  let next_for_iter = next;
  let iter_method = rt
    .alloc_function_value(move |rt, _this, _args| {
      let iterator = rt.alloc_object_value()?;
      let idx_key = rt.property_key_from_str("index")?;
      rt.define_data_property(iterator, idx_key, Value::Number(0.0), true)?;
      let next_key = rt.property_key_from_str("next")?;
      rt.define_data_property(iterator, next_key, next_for_iter, true)?;
      Ok(iterator)
    })
    .unwrap();

  // iterable[Symbol.iterator] = iter_method
  let iter_key = rt.symbol_iterator().unwrap();
  rt.define_data_property(iterable, iter_key, iter_method, true)
    .unwrap();

  let ty = IdlType::Sequence(Box::new(IdlType::Double));
  let out = convert_js_to_idl(&mut rt, &ty, iterable).unwrap();

  let IdlValue::Sequence(values) = out else {
    panic!("expected sequence, got {out:?}");
  };
  let nums = values
    .into_iter()
    .map(|v| match v {
      IdlValue::Double(n) => n,
      other => panic!("expected Double element, got {other:?}"),
    })
    .collect::<Vec<_>>();
  assert_eq!(nums, vec![1.0, 2.0, 3.0]);
}
