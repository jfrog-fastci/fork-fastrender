use fastrender::js::webidl::{
  VmJsWebIdlBindingsCx, VmJsWebIdlBindingsState, WebIdlBindingsRuntime as _,
};
use vm_js::{
  GcObject, Heap, HeapLimits, MicrotaskQueue, NativeFunctionId, PropertyDescriptor, PropertyKey,
  PropertyKind, Scope, Value, Vm, VmError, VmHost, VmHostHooks, VmOptions,
};
use webidl::{InterfaceId, WebIdlHooks, WebIdlLimits};
use webidl_vm_js::bindings_runtime::DataPropertyAttributes;

struct NoHooks;

impl WebIdlHooks<Value> for NoHooks {
  fn is_platform_object(&self, _value: Value) -> bool {
    false
  }

  fn implements_interface(&self, _value: Value, _interface: InterfaceId) -> bool {
    false
  }
}

#[derive(Default, Debug)]
struct SeqHost {
  last: Vec<String>,
}

fn take_sequence<'a>(
  rt: &mut VmJsWebIdlBindingsCx<'a, SeqHost>,
  host: &mut SeqHost,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let v0 = args.get(0).copied().unwrap_or(Value::Undefined);

  if !rt.is_object(v0) {
    return Err(rt.throw_type_error("expected object for sequence<DOMString>"));
  }

  let out: Vec<String> = rt.with_stack_roots(&[v0], |rt| {
    let mut iterator_record = rt.get_iterator(host, v0)?;
    rt.with_stack_roots(&[iterator_record.iterator, iterator_record.next_method], |rt| {
      let mut out = Vec::<String>::new();
      while let Some(next) = rt.iterator_step_value(host, &mut iterator_record)? {
        if out.len() >= rt.limits().max_sequence_length {
          return Err(rt.throw_range_error("sequence exceeds maximum length"));
        }
        let s = rt.to_string(next)?;
        out.push(rt.js_string_to_rust_string(s)?);
      }
      Ok(out)
    })
  })?;

  host.last = out;
  Ok(rt.js_undefined())
}

fn alloc_key(scope: &mut Scope<'_>, name: &str) -> Result<PropertyKey, VmError> {
  let s = scope.alloc_string(name)?;
  scope.push_root(Value::String(s))?;
  Ok(PropertyKey::from_string(s))
}

fn iterator_return_this(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Ok(this)
}

fn iterator_next_call(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(iter_obj) = this else {
    return Err(VmError::TypeError("iterator.next called with non-object receiver"));
  };

  let items_key = alloc_key(scope, "items")?;
  let index_key = alloc_key(scope, "index")?;

  let items = scope
    .heap()
    .object_get_own_data_property_value(iter_obj, &items_key)?
    .unwrap_or(Value::Undefined);
  let Value::Object(items_obj) = items else {
    return Err(VmError::TypeError("iterator items is not object"));
  };

  let index = scope
    .heap()
    .object_get_own_data_property_value(iter_obj, &index_key)?
    .unwrap_or(Value::Number(0.0));
  let idx = match index {
    Value::Number(n) => n as usize,
    _ => 0,
  };

  // Read items.length (array objects store it as an own property in `vm-js`).
  let length_key = alloc_key(scope, "length")?;
  let len_value = scope
    .heap()
    .object_get_own_data_property_value(items_obj, &length_key)?
    .unwrap_or(Value::Number(0.0));
  let len = match len_value {
    Value::Number(n) => n as usize,
    _ => 0,
  };

  let done = idx >= len;
  let value = if done {
    Value::Undefined
  } else {
    let idx_key = alloc_key(scope, &idx.to_string())?;
    scope
      .heap()
      .object_get_own_data_property_value(items_obj, &idx_key)?
      .unwrap_or(Value::Undefined)
  };

  // index++
  scope.heap_mut().object_set_existing_data_property_value(
    iter_obj,
    &index_key,
    Value::Number((idx + 1) as f64),
  )?;

  // Return result object { value, done }.
  let result_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(result_obj))?;

  let value_key = alloc_key(scope, "value")?;
  let done_key = alloc_key(scope, "done")?;
  scope.define_property(
    result_obj,
    value_key,
    PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Data {
        value,
        writable: true,
      },
    },
  )?;
  scope.define_property(
    result_obj,
    done_key,
    PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::Bool(done),
        writable: true,
      },
    },
  )?;

  Ok(Value::Object(result_obj))
}

fn make_custom_iterator(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  object_proto: GcObject,
  sym_iterator: vm_js::GcSymbol,
) -> Result<Value, VmError> {
  // items = ["a", 2, true]
  let a = Value::String(scope.alloc_string("a")?);
  scope.push_root(a)?;
  let items = vm.call_without_host(
    scope,
    Value::Object(vm.intrinsics().unwrap().array_constructor()),
    Value::Undefined,
    &[a, Value::Number(2.0), Value::Bool(true)],
  )?;
  let Value::Object(items_obj) = items else {
    return Err(VmError::InvariantViolation("Array constructor returned non-object"));
  };
  scope.push_root(items)?;

  // iterator = { items, index: 0, next, [Symbol.iterator]: () => this }
  let iterator_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(iterator_obj))?;

  // Ensure the iterator's prototype isn't accidentally treated as an Array by our runtime's
  // conversion fast-path.
  scope
    .heap_mut()
    .object_set_prototype(iterator_obj, Some(object_proto))?;

  let items_key = alloc_key(scope, "items")?;
  scope.define_property(
    iterator_obj,
    items_key,
    PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::Object(items_obj),
        writable: true,
      },
    },
  )?;
  let index_key = alloc_key(scope, "index")?;
  scope.define_property(
    iterator_obj,
    index_key,
    PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::Number(0.0),
        writable: true,
      },
    },
  )?;

  // next()
  let next_id: NativeFunctionId = vm.register_native_call(iterator_next_call)?;
  let next_name = scope.alloc_string("next")?;
  scope.push_root(Value::String(next_name))?;
  let next_fn = scope.alloc_native_function(next_id, None, next_name, 0)?;
  scope.push_root(Value::Object(next_fn))?;
  let next_key = alloc_key(scope, "next")?;
  scope.define_property(
    iterator_obj,
    next_key,
    PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::Object(next_fn),
        writable: true,
      },
    },
  )?;

  // [Symbol.iterator]()
  let iter_id: NativeFunctionId = vm.register_native_call(iterator_return_this)?;
  let iter_name = scope.alloc_string("iterator")?;
  scope.push_root(Value::String(iter_name))?;
  let iter_fn = scope.alloc_native_function(iter_id, None, iter_name, 0)?;
  scope.push_root(Value::Object(iter_fn))?;
  let key = PropertyKey::from_symbol(sym_iterator);
  scope.define_property(
    iterator_obj,
    key,
    PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::Object(iter_fn),
        writable: true,
      },
    },
  )?;

  Ok(Value::Object(iterator_obj))
}

fn assert_range_error(scope: &mut Scope<'_>, realm: &vm_js::Realm, err: VmError) -> Result<(), VmError> {
  let Some(thrown) = err.thrown_value() else {
    return Err(VmError::TypeError("expected thrown JS value"));
  };
  let Value::Object(obj) = thrown else {
    return Err(VmError::TypeError("expected thrown object"));
  };
  let proto = scope.heap().object_prototype(obj)?;
  assert_eq!(proto, Some(realm.intrinsics().range_error_prototype()));
  Ok(())
}

#[test]
fn vmjs_sequence_domstring_via_iterator_protocol_and_limits() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 8 * 1024 * 1024));
  let mut runtime = vm_js::JsRuntime::new(vm, heap)?;

  let mut limits = WebIdlLimits::default();
  limits.max_sequence_length = 8;

  let mut state = Box::new(VmJsWebIdlBindingsState::<SeqHost>::new(
    runtime.realm().global_object(),
    limits,
    Box::new(NoHooks),
  ));

  // Install `takeSequence` on the global object.
  {
    let (vm, heap, _realm) = webidl_vm_js::split_js_runtime(&mut runtime);
    let mut cx = VmJsWebIdlBindingsCx::new(vm, heap, &state);
    let func = cx.create_function("takeSequence", 1, take_sequence)?;
    let global = cx.global_object()?;
    cx.define_data_property_str(
      global,
      "takeSequence",
      func,
      DataPropertyAttributes::new(true, true, true),
    )?;
  }

  let mut host = SeqHost::default();
  let mut hooks = MicrotaskQueue::new();

  // Drive calls directly through the VM.
  let (vm, heap, realm) = webidl_vm_js::split_js_runtime(&mut runtime);
  let mut scope = heap.scope();

  let intr = realm.intrinsics();
  let global = realm.global_object();
  scope.push_root(Value::Object(global))?;

  let take_key = alloc_key(&mut scope, "takeSequence")?;
  let take_fn = vm.get(&mut scope, global, take_key)?;

  // --- array input ----------------------------------------------------------
  let s_x = scope.alloc_string("x")?;
  scope.push_root(Value::String(s_x))?;
  let arr = vm.call_with_host_and_hooks(
    &mut host,
    &mut scope,
    &mut hooks,
    Value::Object(intr.array_constructor()),
    Value::Undefined,
    &[Value::String(s_x), Value::Number(2.0), Value::Bool(true)],
  )?;
  scope.push_root(arr)?;
  vm.call_with_host_and_hooks(&mut host, &mut scope, &mut hooks, take_fn, Value::Undefined, &[arr])?;
  assert_eq!(host.last, vec!["x", "2", "true"]);

  // --- custom iterator input ------------------------------------------------
  let custom_iter = make_custom_iterator(vm, &mut scope, intr.object_prototype(), intr.well_known_symbols().iterator)?;
  scope.push_root(custom_iter)?;
  vm.call_with_host_and_hooks(
    &mut host,
    &mut scope,
    &mut hooks,
    take_fn,
    Value::Undefined,
    &[custom_iter],
  )?;
  assert_eq!(host.last, vec!["a", "2", "true"]);

  // --- limit enforcement ----------------------------------------------------
  state.limits.max_sequence_length = 2;
  let err = vm
    .call_with_host_and_hooks(&mut host, &mut scope, &mut hooks, take_fn, Value::Undefined, &[arr])
    .unwrap_err();
  assert_range_error(&mut scope, realm, err)?;

  Ok(())
}
