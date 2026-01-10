use fastrender::js::bindings::{
  install_worker_bindings, install_window_bindings, BindingValue, WebHostBindings,
};
use fastrender::js::webidl::{
  InterfaceId, VmJsWebIdlBindingsCx, VmJsWebIdlBindingsState, WebIdlHooks, WebIdlLimits,
};
use fastrender::js::webidl::legacy::VmJsRuntime;
use vm_js::{
  ExecutionContext, Heap, HeapLimits, MicrotaskQueue, PropertyKey, PropertyKind, Realm, Scope, Value, Vm,
  VmError, VmOptions,
};
use webidl_js_runtime::JsRuntime as _;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReturnMode {
  Sequence,
  FrozenArray,
}

impl Default for ReturnMode {
  fn default() -> Self {
    Self::Sequence
  }
}

#[derive(Default)]
struct ReturnArrayHost {
  mode: ReturnMode,
}

fn host_return_values(mode: ReturnMode) -> BindingValue<Value> {
  let items = vec![
    BindingValue::String("a".to_string()),
    BindingValue::String("b".to_string()),
  ];
  match mode {
    ReturnMode::Sequence => BindingValue::Sequence(items),
    ReturnMode::FrozenArray => BindingValue::FrozenArray(items),
  }
}

impl WebHostBindings<VmJsRuntime> for ReturnArrayHost {
  fn call_operation(
    &mut self,
    _rt: &mut VmJsRuntime,
    _receiver: Option<Value>,
    interface: &'static str,
    operation: &'static str,
    _overload: usize,
    _args: Vec<BindingValue<Value>>,
  ) -> Result<BindingValue<Value>, VmError> {
    match (interface, operation) {
      ("URLSearchParams", "getAll") => Ok(host_return_values(self.mode)),
      _ => Err(VmError::TypeError("unexpected host call")),
    }
  }
}

fn legacy_get(rt: &mut VmJsRuntime, obj: Value, name: &str) -> Result<Value, VmError> {
  let key = rt.property_key_from_str(name)?;
  rt.get(obj, key)
}

fn legacy_get_method(rt: &mut VmJsRuntime, obj: Value, name: &str) -> Result<Value, VmError> {
  let v = legacy_get(rt, obj, name)?;
  assert!(rt.is_callable(v), "expected {name} to be callable");
  Ok(v)
}

fn value_to_utf8_lossy(rt: &VmJsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string value, got {value:?}");
  };
  rt.heap().get_string(s).unwrap().to_utf8_lossy()
}

fn assert_legacy_array(rt: &mut VmJsRuntime, value: Value, expected: &[&str]) -> Result<(), VmError> {
  let Value::Object(obj) = value else {
    return Err(VmError::TypeError("expected array object"));
  };

  let length_key = rt.property_key_from_str("length")?;
  let length_desc = rt
    .heap()
    .object_get_own_property(obj, &length_key)?
    .expect("expected array to have own length property");
  assert!(
    !length_desc.enumerable,
    "expected array length property to be non-enumerable"
  );
  assert!(
    !length_desc.configurable,
    "expected array length property to be non-configurable"
  );
  match length_desc.kind {
    PropertyKind::Data {
      value: Value::Number(n),
      writable: true,
    } => assert_eq!(n, expected.len() as f64, "array length value"),
    other => panic!("unexpected array length descriptor: {other:?}"),
  };

  for (idx, expected_str) in expected.iter().enumerate() {
    let key = rt.property_key_from_u32(idx as u32)?;
    let actual = rt.get(Value::Object(obj), key)?;
    assert_eq!(value_to_utf8_lossy(rt, actual), *expected_str);
  }

  Ok(())
}

fn run_legacy_install_and_call_get_all(
  install: fn(&mut VmJsRuntime, &mut ReturnArrayHost) -> Result<(), VmError>,
) -> Result<(), VmError> {
  let mut rt = VmJsRuntime::new();
  let mut host = ReturnArrayHost {
    mode: ReturnMode::Sequence,
  };
  install(&mut rt, &mut host)?;

  let global = <VmJsRuntime as fastrender::js::webidl::WebIdlBindingsRuntime<ReturnArrayHost>>::global_object(&mut rt)?;

  let ctor = legacy_get_method(&mut rt, global, "URLSearchParams")?;
  let proto = legacy_get(&mut rt, ctor, "prototype")?;
  let get_all = legacy_get_method(&mut rt, proto, "getAll")?;

  for mode in [ReturnMode::Sequence, ReturnMode::FrozenArray] {
    host.mode = mode;
    let name = rt.alloc_string_value("name")?;
    let out = rt.with_host_context(&mut host, |rt| rt.call(get_all, global, &[name]))?;
    assert_legacy_array(&mut rt, out, &["a", "b"])?;
  }

  Ok(())
}

struct NoHooks;

impl WebIdlHooks<Value> for NoHooks {
  fn is_platform_object(&self, _value: Value) -> bool {
    false
  }

  fn implements_interface(&self, _value: Value, _interface: InterfaceId) -> bool {
    false
  }
}

impl<'a> WebHostBindings<VmJsWebIdlBindingsCx<'a, ReturnArrayHost>> for ReturnArrayHost {
  fn call_operation(
    &mut self,
    _rt: &mut VmJsWebIdlBindingsCx<'a, ReturnArrayHost>,
    _receiver: Option<Value>,
    interface: &'static str,
    operation: &'static str,
    _overload: usize,
    _args: Vec<BindingValue<Value>>,
  ) -> Result<BindingValue<Value>, VmError> {
    match (interface, operation) {
      ("URLSearchParams", "getAll") => Ok(host_return_values(self.mode)),
      _ => Err(VmError::TypeError("unexpected host call")),
    }
  }
}

fn alloc_key(scope: &mut Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
}

fn assert_realm_array(
  scope: &mut Scope<'_>,
  vm: &mut Vm,
  arr: Value,
  expected: &[&str],
) -> Result<(), VmError> {
  let Value::Object(arr_obj) = arr else {
    return Err(VmError::TypeError("expected array object"));
  };

  let intr = vm
    .intrinsics()
    .expect("expected intrinsics to be installed on realm VM");
  assert_eq!(
    scope.heap().object_prototype(arr_obj)?,
    Some(intr.array_prototype()),
    "expected array [[Prototype]] to be %Array.prototype%"
  );

  let length_key = alloc_key(scope, "length")?;
  let length_desc = scope
    .heap()
    .object_get_own_property(arr_obj, &length_key)?
    .expect("expected array to have own length property");
  assert!(
    !length_desc.enumerable,
    "expected array length property to be non-enumerable"
  );
  assert!(
    !length_desc.configurable,
    "expected array length property to be non-configurable"
  );
  match length_desc.kind {
    PropertyKind::Data {
      value: Value::Number(n),
      writable: true,
    } => assert_eq!(n, expected.len() as f64, "array length value"),
    other => panic!("unexpected array length descriptor: {other:?}"),
  };

  for (idx, expected_str) in expected.iter().enumerate() {
    let key = alloc_key(scope, &idx.to_string())?;
    let value = vm.get(scope, arr_obj, key)?;
    let Value::String(s) = value else {
      panic!("expected string at index {idx}, got {value:?}");
    };
    assert_eq!(scope.heap().get_string(s)?.to_utf8_lossy(), *expected_str);
  }

  Ok(())
}

fn run_realm_install_and_call_get_all() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(64 * 1024 * 1024, 64 * 1024 * 1024));
  let mut vm = Vm::new(VmOptions::default());
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| -> Result<(), VmError> {
    let state = Box::new(VmJsWebIdlBindingsState::<ReturnArrayHost>::new(
      realm.global_object(),
      WebIdlLimits::default(),
      Box::new(NoHooks),
    ));

    let mut host = ReturnArrayHost {
      mode: ReturnMode::Sequence,
    };
    {
      let mut rt = VmJsWebIdlBindingsCx::new(&mut vm, &mut heap, &state);
      install_window_bindings(&mut rt, &mut host)?;
    }

    let mut hooks = MicrotaskQueue::new();
    let mut scope = heap.scope();
    let global_obj = realm.global_object();
    scope.push_root(Value::Object(global_obj))?;

    let mut vm = vm.execution_context_guard(ExecutionContext {
      realm: realm.id(),
      script_or_module: None,
    });

    let ctor_key = alloc_key(&mut scope, "URLSearchParams")?;
    let ctor = vm.get(&mut scope, global_obj, ctor_key)?;
    scope.push_root(ctor)?;
    let Value::Object(ctor_obj) = ctor else {
      panic!("expected URLSearchParams constructor to be an object");
    };

    let proto_key = alloc_key(&mut scope, "prototype")?;
    let proto = vm.get(&mut scope, ctor_obj, proto_key)?;
    scope.push_root(proto)?;
    let Value::Object(proto_obj) = proto else {
      panic!("expected URLSearchParams.prototype to be an object");
    };

    let get_all_key = alloc_key(&mut scope, "getAll")?;
    let get_all = vm.get(&mut scope, proto_obj, get_all_key)?;
    scope.push_root(get_all)?;

    for mode in [ReturnMode::Sequence, ReturnMode::FrozenArray] {
      host.mode = mode;
      let name_s = scope.alloc_string("name")?;
      scope.push_root(Value::String(name_s))?;
      let out = vm.call_with_host_and_hooks(
        &mut host,
        &mut scope,
        &mut hooks,
        get_all,
        Value::Object(global_obj),
        &[Value::String(name_s)],
      )?;
      scope.push_root(out)?;
      assert_realm_array(&mut scope, &mut vm, out, &["a", "b"])?;
    }

    drop(scope);
    drop(hooks);
    drop(host);
    drop(state);

    Ok(())
  }));

  realm.teardown(&mut heap);

  match result {
    Ok(inner) => inner,
    Err(panic) => std::panic::resume_unwind(panic),
  }
}

#[test]
fn webidl_binding_value_sequence_and_frozen_array_convert_to_js_arrays() -> Result<(), VmError> {
  run_legacy_install_and_call_get_all(install_window_bindings::<ReturnArrayHost, VmJsRuntime>)?;
  run_legacy_install_and_call_get_all(install_worker_bindings::<ReturnArrayHost, VmJsRuntime>)?;
  run_realm_install_and_call_get_all()?;
  Ok(())
}
