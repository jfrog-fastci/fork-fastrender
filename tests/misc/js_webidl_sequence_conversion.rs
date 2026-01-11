use std::cell::Cell;
use std::rc::Rc;

use fastrender::js::bindings::{BindingValue, WebHostBindings};
use fastrender::js::webidl::WebIdlLimits;
use vm_js::{HeapLimits, PropertyKey, Value, VmError};
use webidl_js_runtime::{JsRuntime as _, VmJsRuntime};

struct SeqHost {
  expected_operation: &'static str,
  called: bool,
  received: Vec<BindingValue<Value>>,
}

impl SeqHost {
  fn new(expected_operation: &'static str) -> Self {
    Self {
      expected_operation,
      called: false,
      received: Vec::new(),
    }
  }
}

impl WebHostBindings<VmJsRuntime> for SeqHost {
  fn call_operation(
    &mut self,
    _rt: &mut VmJsRuntime,
    _receiver: Option<Value>,
    interface: &'static str,
    operation: &'static str,
    _overload: usize,
    args: Vec<BindingValue<Value>>,
  ) -> Result<BindingValue<Value>, VmError> {
    assert_eq!(interface, "Test");
    assert_eq!(operation, self.expected_operation);
    self.called = true;
    self.received = args;
    Ok(BindingValue::Undefined)
  }
}

fn make_numeric_iterable(rt: &mut VmJsRuntime, values: Vec<Value>) -> Result<Value, VmError> {
  let iterator_obj = rt.alloc_object_value()?;

  let idx = Rc::new(Cell::new(0usize));
  let values = Rc::new(values);
  let idx_for_next = idx.clone();
  let values_for_next = values.clone();

  // With GC pressure (we force a collection before each allocation), any heap handles that are only
  // held in Rust locals/closures must be explicitly rooted while we wire up the iterator objects.
  rt.with_stack_roots(&[iterator_obj], |rt| {
    let next_fn = rt.alloc_function_value(move |rt, _this, _args| {
      let i = idx_for_next.get();
      let result_obj = rt.alloc_object_value()?;
      rt.with_stack_roots(&[result_obj], |rt| {
        let done_key = rt.property_key_from_str("done")?;
        if i >= values_for_next.len() {
          webidl_js_runtime::JsRuntime::define_data_property(
            rt,
            result_obj,
            done_key,
            Value::Bool(true),
            true,
          )?;
          let value_key = rt.property_key_from_str("value")?;
          webidl_js_runtime::JsRuntime::define_data_property(
            rt,
            result_obj,
            value_key,
            Value::Undefined,
            true,
          )?;
        } else {
          webidl_js_runtime::JsRuntime::define_data_property(
            rt,
            result_obj,
            done_key,
            Value::Bool(false),
            true,
          )?;
          let value_key = rt.property_key_from_str("value")?;
          webidl_js_runtime::JsRuntime::define_data_property(
            rt,
            result_obj,
            value_key,
            values_for_next[i],
            true,
          )?;
          idx_for_next.set(i + 1);
        }
        Ok(())
      })?;
      Ok(result_obj)
    })?;

    rt.with_stack_roots(&[iterator_obj, next_fn], |rt| {
      let next_key = rt.property_key_from_str("next")?;
      webidl_js_runtime::JsRuntime::define_data_property(rt, iterator_obj, next_key, next_fn, true)
    })?;

    // Root `iterator_obj` by storing it on the iterable object (host closures do not participate in
    // `vm-js` GC tracing).
    let iterable_obj = rt.alloc_object_value()?;
    rt.with_stack_roots(&[iterable_obj, iterator_obj], |rt| {
      let iter_holder_key = rt.property_key_from_str("_iter")?;
      webidl_js_runtime::JsRuntime::define_data_property(
        rt,
        iterable_obj,
        iter_holder_key,
        iterator_obj,
        false,
      )?;

      let iterator_getter = rt.alloc_function_value(move |_rt, _this, _args| Ok(iterator_obj))?;
      rt.with_stack_roots(&[iterable_obj, iterator_getter], |rt| {
        let iterator_sym = webidl_js_runtime::WebIdlJsRuntime::symbol_iterator(rt)?;
        webidl_js_runtime::JsRuntime::define_data_property(
          rt,
          iterable_obj,
          iterator_sym,
          iterator_getter,
          true,
        )?;
        Ok(())
      })?;

      Ok(())
    })?;

    Ok(iterable_obj)
  })
}

fn takes_sequence_wrapper<Host, R>(
  rt: &mut R,
  host: &mut Host,
  _this: R::JsValue,
  args: &[R::JsValue],
) -> Result<R::JsValue, R::Error>
where
  R: fastrender::js::webidl::WebIdlBindingsRuntime<Host>,
  Host: WebHostBindings<R>,
{
  let mut converted_args: Vec<BindingValue<R::JsValue>> = Vec::new();
  let v0 = if args.len() > 0 { args[0] } else { rt.js_undefined() };

  // This is the shape emitted by the bindings codegen for `sequence<long>`.
  converted_args.push({
    if !rt.is_object(v0) {
      return Err(rt.throw_type_error("expected object for sequence"));
    }
    rt.with_stack_roots(&[v0], |rt| {
      let mut iterator_record = rt.get_iterator(host, v0)?;
      rt.with_stack_roots(&[iterator_record.iterator, iterator_record.next_method], |rt| {
        let mut values: Vec<BindingValue<R::JsValue>> = Vec::new();
        while let Some(next) = rt.iterator_step_value(host, &mut iterator_record)? {
          if values.len() >= rt.limits().max_sequence_length {
            return Err(rt.throw_range_error("sequence exceeds maximum length"));
          }
          let converted =
            rt.with_stack_roots(&[next], |rt| Ok(BindingValue::Number(rt.to_number(host, next)?)))?;
          values.push(converted);
        }
        Ok(BindingValue::Sequence(values))
      })
    })?
  });

  let _ = host.call_operation(rt, None, "Test", "takesSequence", 0, converted_args)?;
  Ok(rt.js_undefined())
}

fn takes_frozen_array_wrapper<Host, R>(
  rt: &mut R,
  host: &mut Host,
  _this: R::JsValue,
  args: &[R::JsValue],
) -> Result<R::JsValue, R::Error>
where
  R: fastrender::js::webidl::WebIdlBindingsRuntime<Host>,
  Host: WebHostBindings<R>,
{
  let mut converted_args: Vec<BindingValue<R::JsValue>> = Vec::new();
  let v0 = if args.len() > 0 { args[0] } else { rt.js_undefined() };

  // This is the shape emitted by the bindings codegen for `FrozenArray<long>`.
  converted_args.push({
    if !rt.is_object(v0) {
      return Err(rt.throw_type_error("expected object for FrozenArray"));
    }
    rt.with_stack_roots(&[v0], |rt| {
      let mut iterator_record = rt.get_iterator(host, v0)?;
      rt.with_stack_roots(&[iterator_record.iterator, iterator_record.next_method], |rt| {
        let mut values: Vec<BindingValue<R::JsValue>> = Vec::new();
        while let Some(next) = rt.iterator_step_value(host, &mut iterator_record)? {
          if values.len() >= rt.limits().max_sequence_length {
            return Err(rt.throw_range_error("FrozenArray exceeds maximum length"));
          }
          let converted =
            rt.with_stack_roots(&[next], |rt| Ok(BindingValue::Number(rt.to_number(host, next)?)))?;
          values.push(converted);
        }
        Ok(BindingValue::FrozenArray(values))
      })
    })?
  });

  let _ = host.call_operation(rt, None, "Test", "takesFrozenArray", 0, converted_args)?;
  Ok(rt.js_undefined())
}

fn thrown_error_name(rt: &mut VmJsRuntime, err: VmError) -> Result<String, VmError> {
  let Some(thrown) = err.thrown_value() else {
    return Err(VmError::TypeError("expected thrown error"));
  };
  rt.with_stack_roots(&[thrown], |rt| {
    let name_key: PropertyKey = rt.property_key_from_str("name")?;
    let name_value = rt.get(thrown, name_key)?;
    rt.with_stack_roots(&[name_value], |rt| {
      let s = rt.to_string(name_value)?;
      rt.string_to_utf8_lossy(s)
    })
  })
}

#[test]
fn generated_bindings_convert_sequence_long_from_iterable() -> Result<(), VmError> {
  // Stress rooting: force a GC before each allocation.
  let mut rt = VmJsRuntime::with_limits(HeapLimits::new(1024 * 1024, 0));
  let mut host = SeqHost::new("takesSequence");

  let func =
    <VmJsRuntime as fastrender::js::webidl::WebIdlBindingsRuntime<SeqHost>>::create_function(
      &mut rt,
      "takesSequence",
      1,
      takes_sequence_wrapper::<SeqHost, VmJsRuntime>,
    )?;
  // Keep the wrapper function alive across the allocations performed while constructing the test
  // iterable below.
  let _func_root = rt.heap_mut().add_root(func)?;

  let iterable = make_numeric_iterable(&mut rt, vec![Value::Number(1.0), Value::Number(2.0)])?;
  rt.with_host_context(&mut host, |rt| {
    let this = rt.js_undefined();
    rt.call(func, this, &[iterable])
  })?;

  assert!(host.called);
  assert_eq!(host.received.len(), 1);
  let BindingValue::Sequence(values) = &host.received[0] else {
    panic!("expected BindingValue::Sequence");
  };
  assert_eq!(values.len(), 2);
  let BindingValue::Number(n0) = &values[0] else {
    panic!("expected first element to be BindingValue::Number");
  };
  assert_eq!(*n0, 1.0);
  let BindingValue::Number(n1) = &values[1] else {
    panic!("expected second element to be BindingValue::Number");
  };
  assert_eq!(*n1, 2.0);
  Ok(())
}

#[test]
fn generated_bindings_convert_sequence_long_from_array() -> Result<(), VmError> {
  // Stress rooting: force a GC before each allocation.
  let mut rt = VmJsRuntime::with_limits(HeapLimits::new(1024 * 1024, 0));
  let mut host = SeqHost::new("takesSequence");

  let func =
    <VmJsRuntime as fastrender::js::webidl::WebIdlBindingsRuntime<SeqHost>>::create_function(
      &mut rt,
      "takesSequence",
      1,
      takes_sequence_wrapper::<SeqHost, VmJsRuntime>,
    )?;
  let _func_root = rt.heap_mut().add_root(func)?;

  let array_obj = {
    let mut scope = rt.heap_mut().scope();
    scope.alloc_array(2)?
  };
  let array = Value::Object(array_obj);
  rt.with_stack_roots(&[array], |rt| {
    let key0 = rt.property_key_from_str("0")?;
    webidl_js_runtime::JsRuntime::define_data_property(rt, array, key0, Value::Number(1.0), true)?;
    let key1 = rt.property_key_from_str("1")?;
    webidl_js_runtime::JsRuntime::define_data_property(rt, array, key1, Value::Number(2.0), true)?;
    Ok(())
  })?;

  rt.with_host_context(&mut host, |rt| {
    let this = rt.js_undefined();
    rt.call(func, this, &[array])
  })?;

  assert!(host.called);
  assert_eq!(host.received.len(), 1);
  let BindingValue::Sequence(values) = &host.received[0] else {
    panic!("expected BindingValue::Sequence");
  };
  assert_eq!(values.len(), 2);
  let BindingValue::Number(n0) = &values[0] else {
    panic!("expected first element to be BindingValue::Number");
  };
  assert_eq!(*n0, 1.0);
  let BindingValue::Number(n1) = &values[1] else {
    panic!("expected second element to be BindingValue::Number");
  };
  assert_eq!(*n1, 2.0);
  Ok(())
}

#[test]
fn generated_bindings_convert_frozen_array_long_from_iterable() -> Result<(), VmError> {
  // Stress rooting: force a GC before each allocation.
  let mut rt = VmJsRuntime::with_limits(HeapLimits::new(1024 * 1024, 0));
  let mut host = SeqHost::new("takesFrozenArray");

  let func =
    <VmJsRuntime as fastrender::js::webidl::WebIdlBindingsRuntime<SeqHost>>::create_function(
      &mut rt,
      "takesFrozenArray",
      1,
      takes_frozen_array_wrapper::<SeqHost, VmJsRuntime>,
    )?;
  // Keep the wrapper function alive across the allocations performed while constructing the test
  // iterable below.
  let _func_root = rt.heap_mut().add_root(func)?;

  let iterable = make_numeric_iterable(&mut rt, vec![Value::Number(1.0), Value::Number(2.0)])?;
  rt.with_host_context(&mut host, |rt| {
    let this = rt.js_undefined();
    rt.call(func, this, &[iterable])
  })?;

  assert!(host.called);
  assert_eq!(host.received.len(), 1);
  let BindingValue::FrozenArray(values) = &host.received[0] else {
    panic!("expected BindingValue::FrozenArray");
  };
  assert_eq!(values.len(), 2);
  let BindingValue::Number(n0) = &values[0] else {
    panic!("expected first element to be BindingValue::Number");
  };
  assert_eq!(*n0, 1.0);
  let BindingValue::Number(n1) = &values[1] else {
    panic!("expected second element to be BindingValue::Number");
  };
  assert_eq!(*n1, 2.0);
  Ok(())
}

#[test]
fn generated_bindings_enforce_max_sequence_length() -> Result<(), VmError> {
  // Stress rooting: force a GC before each allocation.
  let mut rt = VmJsRuntime::with_limits(HeapLimits::new(1024 * 1024, 0));
  rt.set_webidl_limits(WebIdlLimits {
    max_sequence_length: 1,
    ..WebIdlLimits::default()
  });
  let mut host = SeqHost::new("takesSequence");

  let func =
    <VmJsRuntime as fastrender::js::webidl::WebIdlBindingsRuntime<SeqHost>>::create_function(
      &mut rt,
      "takesSequence",
      1,
      takes_sequence_wrapper::<SeqHost, VmJsRuntime>,
    )?;
  let _func_root = rt.heap_mut().add_root(func)?;

  let iterable = make_numeric_iterable(&mut rt, vec![Value::Number(1.0), Value::Number(2.0)])?;
  let err = rt
    .with_host_context(&mut host, |rt| {
      let this = rt.js_undefined();
      rt.call(func, this, &[iterable])
    })
    .expect_err("expected conversion to throw");
  assert_eq!(thrown_error_name(&mut rt, err)?, "RangeError");
  assert!(!host.called, "host should not be called on conversion error");
  Ok(())
}

#[test]
fn generated_bindings_sequence_conversion_throws_type_error_on_non_object() -> Result<(), VmError> {
  // Stress rooting: force a GC before each allocation.
  let mut rt = VmJsRuntime::with_limits(HeapLimits::new(1024 * 1024, 0));
  let mut host = SeqHost::new("takesSequence");

  let func =
    <VmJsRuntime as fastrender::js::webidl::WebIdlBindingsRuntime<SeqHost>>::create_function(
      &mut rt,
      "takesSequence",
      1,
      takes_sequence_wrapper::<SeqHost, VmJsRuntime>,
    )?;
  let _func_root = rt.heap_mut().add_root(func)?;

  let err = rt
    .with_host_context(&mut host, |rt| {
      let this = rt.js_undefined();
      rt.call(func, this, &[Value::Number(1.0)])
    })
    .expect_err("expected conversion to throw");
  assert_eq!(thrown_error_name(&mut rt, err)?, "TypeError");
  assert!(!host.called, "host should not be called on conversion error");
  Ok(())
}

#[test]
fn generated_bindings_sequence_conversion_throws_type_error_on_non_iterable() -> Result<(), VmError> {
  // Stress rooting: force a GC before each allocation.
  let mut rt = VmJsRuntime::with_limits(HeapLimits::new(1024 * 1024, 0));
  let mut host = SeqHost::new("takesSequence");

  let func =
    <VmJsRuntime as fastrender::js::webidl::WebIdlBindingsRuntime<SeqHost>>::create_function(
      &mut rt,
      "takesSequence",
      1,
      takes_sequence_wrapper::<SeqHost, VmJsRuntime>,
    )?;
  let _func_root = rt.heap_mut().add_root(func)?;

  let obj = rt.alloc_object_value()?;
  let err = rt
    .with_host_context(&mut host, |rt| {
      let this = rt.js_undefined();
      rt.call(func, this, &[obj])
    })
    .expect_err("expected conversion to throw");
  assert_eq!(thrown_error_name(&mut rt, err)?, "TypeError");
  assert!(!host.called, "host should not be called on conversion error");
  Ok(())
}

#[test]
fn generated_bindings_sequence_conversion_throws_type_error_on_non_callable_iterator() -> Result<(), VmError> {
  // Stress rooting: force a GC before each allocation.
  let mut rt = VmJsRuntime::with_limits(HeapLimits::new(1024 * 1024, 0));
  let mut host = SeqHost::new("takesSequence");

  let func =
    <VmJsRuntime as fastrender::js::webidl::WebIdlBindingsRuntime<SeqHost>>::create_function(
      &mut rt,
      "takesSequence",
      1,
      takes_sequence_wrapper::<SeqHost, VmJsRuntime>,
    )?;
  let _func_root = rt.heap_mut().add_root(func)?;

  let obj = rt.alloc_object_value()?;
  rt.with_stack_roots(&[obj], |rt| {
    let iterator_sym = webidl_js_runtime::WebIdlJsRuntime::symbol_iterator(rt)?;
    webidl_js_runtime::JsRuntime::define_data_property(
      rt,
      obj,
      iterator_sym,
      Value::Number(1.0),
      true,
    )
  })?;

  let err = rt
    .with_host_context(&mut host, |rt| {
      let this = rt.js_undefined();
      rt.call(func, this, &[obj])
    })
    .expect_err("expected conversion to throw");
  assert_eq!(thrown_error_name(&mut rt, err)?, "TypeError");
  assert!(!host.called, "host should not be called on conversion error");
  Ok(())
}

#[test]
fn generated_bindings_frozen_array_conversion_throws_type_error_on_non_object() -> Result<(), VmError> {
  // Stress rooting: force a GC before each allocation.
  let mut rt = VmJsRuntime::with_limits(HeapLimits::new(1024 * 1024, 0));
  let mut host = SeqHost::new("takesFrozenArray");

  let func =
    <VmJsRuntime as fastrender::js::webidl::WebIdlBindingsRuntime<SeqHost>>::create_function(
      &mut rt,
      "takesFrozenArray",
      1,
      takes_frozen_array_wrapper::<SeqHost, VmJsRuntime>,
    )?;
  let _func_root = rt.heap_mut().add_root(func)?;

  let err = rt
    .with_host_context(&mut host, |rt| {
      let this = rt.js_undefined();
      rt.call(func, this, &[Value::Number(1.0)])
    })
    .expect_err("expected conversion to throw");
  assert_eq!(thrown_error_name(&mut rt, err)?, "TypeError");
  assert!(!host.called, "host should not be called on conversion error");
  Ok(())
}

#[test]
fn generated_bindings_frozen_array_conversion_throws_type_error_on_non_iterable() -> Result<(), VmError> {
  // Stress rooting: force a GC before each allocation.
  let mut rt = VmJsRuntime::with_limits(HeapLimits::new(1024 * 1024, 0));
  let mut host = SeqHost::new("takesFrozenArray");

  let func =
    <VmJsRuntime as fastrender::js::webidl::WebIdlBindingsRuntime<SeqHost>>::create_function(
      &mut rt,
      "takesFrozenArray",
      1,
      takes_frozen_array_wrapper::<SeqHost, VmJsRuntime>,
    )?;
  let _func_root = rt.heap_mut().add_root(func)?;

  let obj = rt.alloc_object_value()?;
  let err = rt
    .with_host_context(&mut host, |rt| {
      let this = rt.js_undefined();
      rt.call(func, this, &[obj])
    })
    .expect_err("expected conversion to throw");
  assert_eq!(thrown_error_name(&mut rt, err)?, "TypeError");
  assert!(!host.called, "host should not be called on conversion error");
  Ok(())
}

#[test]
fn generated_bindings_frozen_array_conversion_throws_type_error_on_non_callable_iterator() -> Result<(), VmError> {
  // Stress rooting: force a GC before each allocation.
  let mut rt = VmJsRuntime::with_limits(HeapLimits::new(1024 * 1024, 0));
  let mut host = SeqHost::new("takesFrozenArray");

  let func =
    <VmJsRuntime as fastrender::js::webidl::WebIdlBindingsRuntime<SeqHost>>::create_function(
      &mut rt,
      "takesFrozenArray",
      1,
      takes_frozen_array_wrapper::<SeqHost, VmJsRuntime>,
    )?;
  let _func_root = rt.heap_mut().add_root(func)?;

  let obj = rt.alloc_object_value()?;
  rt.with_stack_roots(&[obj], |rt| {
    let iterator_sym = webidl_js_runtime::WebIdlJsRuntime::symbol_iterator(rt)?;
    webidl_js_runtime::JsRuntime::define_data_property(
      rt,
      obj,
      iterator_sym,
      Value::Number(1.0),
      true,
    )
  })?;

  let err = rt
    .with_host_context(&mut host, |rt| {
      let this = rt.js_undefined();
      rt.call(func, this, &[obj])
    })
    .expect_err("expected conversion to throw");
  assert_eq!(thrown_error_name(&mut rt, err)?, "TypeError");
  assert!(!host.called, "host should not be called on conversion error");
  Ok(())
}

#[test]
fn generated_bindings_enforce_max_frozen_array_length() -> Result<(), VmError> {
  // Stress rooting: force a GC before each allocation.
  let mut rt = VmJsRuntime::with_limits(HeapLimits::new(1024 * 1024, 0));
  rt.set_webidl_limits(WebIdlLimits {
    max_sequence_length: 1,
    ..WebIdlLimits::default()
  });
  let mut host = SeqHost::new("takesFrozenArray");

  let func =
    <VmJsRuntime as fastrender::js::webidl::WebIdlBindingsRuntime<SeqHost>>::create_function(
      &mut rt,
      "takesFrozenArray",
      1,
      takes_frozen_array_wrapper::<SeqHost, VmJsRuntime>,
    )?;
  let _func_root = rt.heap_mut().add_root(func)?;

  let iterable = make_numeric_iterable(&mut rt, vec![Value::Number(1.0), Value::Number(2.0)])?;
  let err = rt
    .with_host_context(&mut host, |rt| {
      let this = rt.js_undefined();
      rt.call(func, this, &[iterable])
    })
    .expect_err("expected conversion to throw");
  assert_eq!(thrown_error_name(&mut rt, err)?, "RangeError");
  assert!(!host.called, "host should not be called on conversion error");
  Ok(())
}
