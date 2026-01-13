use vm_js::{
  GcObject, Heap, HeapLimits, PropertyDescriptor, PropertyKey, PropertyKind, Realm, Scope, Value,
  Vm, VmError, VmHost, VmHostHooks, VmOptions,
  JsRuntime,
};

struct TestRealm {
  vm: Vm,
  heap: Heap,
  realm: Realm,
}

impl TestRealm {
  fn new() -> Result<Self, VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let realm = Realm::new(&mut vm, &mut heap)?;
    Ok(Self { vm, heap, realm })
  }
}

impl Drop for TestRealm {
  fn drop(&mut self) {
    self.realm.teardown(&mut self.heap);
  }
}

fn new_runtime() -> Result<JsRuntime, VmError> {
  let vm = Vm::new(VmOptions::default());
  // `JsRuntime::exec_script` needs enough heap headroom for parsing/compiling the test script
  // itself in addition to the objects it creates at runtime.
  let heap = Heap::new(HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024));
  JsRuntime::new(vm, heap)
}

fn as_utf8_lossy(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap().get_string(s).unwrap().to_utf8_lossy()
}

fn get_own_data_property(
  scope: &mut Scope<'_>,
  obj: GcObject,
  name: &str,
) -> Result<Option<Value>, VmError> {
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;
  let key = PropertyKey::from_string(scope.alloc_string(name)?);
  scope.heap().object_get_own_data_property_value(obj, &key)
}

fn define_data_property(
  scope: &mut Scope<'_>,
  obj: GcObject,
  name: &str,
  value: Value,
) -> Result<(), VmError> {
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;
  scope.push_root(value)?;
  let key = PropertyKey::from_string(scope.alloc_string(name)?);
  let desc = PropertyDescriptor {
    enumerable: true,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: true,
    },
  };
  scope.define_property(obj, key, desc)
}

fn define_accessor_property(
  scope: &mut Scope<'_>,
  obj: GcObject,
  name: &str,
  get: Value,
) -> Result<(), VmError> {
  let mut scope = scope.reborrow();
  scope.push_root(Value::Object(obj))?;
  scope.push_root(get)?;
  let key = PropertyKey::from_string(scope.alloc_string(name)?);
  let desc = PropertyDescriptor {
    enumerable: true,
    configurable: true,
    kind: PropertyKind::Accessor {
      get,
      set: Value::Undefined,
    },
  };
  scope.define_property(obj, key, desc)
}

fn return_this_native(
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

fn throw_type_error_native(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError("boom"))
}

fn iterator_return_set_closed_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(iterator) = this else {
    return Err(VmError::TypeError("iterator.return this is not an object"));
  };
  define_data_property(scope, iterator, "closed", Value::Bool(true))?;
  Ok(Value::Object(iterator))
}

fn iterator_return_set_closed_returns_undefined_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Value::Object(iterator) = this else {
    return Err(VmError::TypeError("iterator.return this is not an object"));
  };
  define_data_property(scope, iterator, "closed", Value::Bool(true))?;
  Ok(Value::Undefined)
}

fn throw_slot0_native(
  _vm: &mut Vm,
  scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let slots = scope.heap().get_function_native_slots(callee)?;
  let Some(value) = slots.get(0).copied() else {
    return Err(VmError::InvariantViolation("throw_slot0_native missing slot 0"));
  };
  Err(VmError::Throw(value))
}

fn iterator_next_throw_native(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Err(VmError::TypeError("next throws"))
}

fn iterator_next_return_null_native(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Ok(Value::Null)
}

fn iterator_next_return_result_native(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let mut scope = scope.reborrow();
  scope.push_root(this)?;
  let Value::Object(iterator) = this else {
    return Err(VmError::TypeError("iterator.next this is not an object"));
  };
  let result_key = PropertyKey::from_string(scope.alloc_string("result")?);
  scope.ordinary_get_with_host_and_hooks(vm, host, hooks, iterator, result_key, this)
}

fn require_closed_flag(scope: &mut Scope<'_>, iterator: GcObject, expected: bool) -> Result<(), VmError> {
  assert_eq!(
    get_own_data_property(scope, iterator, "closed")?,
    Some(Value::Bool(expected))
  );
  Ok(())
}

#[test]
fn object_from_entries_evaluation_order() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let value = rt.exec_script(
    r#"
      (function () {
        var effects = [];

        function makeEntry(label) {
          return {
            get '0'() {
              effects.push('access property "0" of ' + label + ' entry');
              return {
                toString: function() {
                  effects.push('toString of ' + label + ' key');
                  return label + ' key';
                },
              };
            },
            get '1'() {
              effects.push('access property "1" of ' + label + ' entry');
              return label + ' value';
            },
          };
        }

        var iterable = {
          [Symbol.iterator]: function() {
            effects.push('get Symbol.iterator');
            var count = 0;
            return {
              next: function() {
                effects.push('next ' + count);
                if (count === 0) {
                  ++count;
                  return { done: false, value: makeEntry('first') };
                } else if (count === 1) {
                  ++count;
                  return { done: false, value: makeEntry('second') };
                } else {
                  return { done: true };
                }
              },
            };
          },
        };

        var result = Object.fromEntries(iterable);
        return effects.join('|') + ';' + result['first key'] + ';' + result['second key'];
      })()
    "#,
  )?;

  assert_eq!(
    as_utf8_lossy(&rt, value),
    "get Symbol.iterator|next 0|access property \"0\" of first entry|access property \"1\" of first entry|toString of first key|next 1|access property \"0\" of second entry|access property \"1\" of second entry|toString of second key|next 2;first value;second value"
  );

  Ok(())
}

#[test]
fn object_from_entries_iterator_not_closed_for_throwing_next() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;
  let intr = *rt.realm.intrinsics();
  let object = intr.object_constructor();

  let mut scope = rt.heap.scope();

  let from_entries = get_own_data_property(&mut scope, object, "fromEntries")?.unwrap();
  let Value::Object(from_entries) = from_entries else {
    return Err(VmError::Unimplemented("Object.fromEntries is not a function object"));
  };

  let iter = scope.alloc_object()?;
  scope.push_root(Value::Object(iter))?;
  define_data_property(&mut scope, iter, "closed", Value::Bool(false))?;

  let return_this_id = rt.vm.register_native_call(return_this_native)?;
  let throw_next_id = rt.vm.register_native_call(iterator_next_throw_native)?;
  let return_set_closed_id = rt.vm.register_native_call(iterator_return_set_closed_native)?;

  let fn_name = scope.alloc_string("")?;
  let iter_method = scope.alloc_native_function(return_this_id, None, fn_name, 0)?;
  scope.push_root(Value::Object(iter_method))?;
  let next_name = scope.alloc_string("")?;
  let next = scope.alloc_native_function(throw_next_id, None, next_name, 0)?;
  scope.push_root(Value::Object(next))?;
  let return_name = scope.alloc_string("")?;
  let return_ = scope.alloc_native_function(return_set_closed_id, None, return_name, 0)?;
  scope.push_root(Value::Object(return_))?;

  let iterator_key = PropertyKey::from_symbol(intr.well_known_symbols().iterator);
  scope.define_property(
    iter,
    iterator_key,
    PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::Object(iter_method),
        writable: true,
      },
    },
  )?;
  define_data_property(&mut scope, iter, "next", Value::Object(next))?;
  define_data_property(&mut scope, iter, "return", Value::Object(return_))?;

  let args = [Value::Object(iter)];
  let _err = rt
    .vm
    .call_without_host(&mut scope, Value::Object(from_entries), Value::Object(object), &args)
    .unwrap_err();

  require_closed_flag(&mut scope, iter, false)?;
  Ok(())
}

#[test]
fn object_from_entries_iterator_not_closed_for_throwing_done_accessor() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;
  let intr = *rt.realm.intrinsics();
  let object = intr.object_constructor();

  let mut scope = rt.heap.scope();

  let from_entries = get_own_data_property(&mut scope, object, "fromEntries")?.unwrap();
  let Value::Object(from_entries) = from_entries else {
    return Err(VmError::Unimplemented("Object.fromEntries is not a function object"));
  };

  let iter = scope.alloc_object()?;
  scope.push_root(Value::Object(iter))?;
  define_data_property(&mut scope, iter, "closed", Value::Bool(false))?;

  let iter_result = scope.alloc_object()?;
  scope.push_root(Value::Object(iter_result))?;
  let throw_id = rt.vm.register_native_call(throw_type_error_native)?;
  let throw_name = scope.alloc_string("")?;
  let throw_fn = scope.alloc_native_function(throw_id, None, throw_name, 0)?;
  scope.push_root(Value::Object(throw_fn))?;
  define_accessor_property(&mut scope, iter_result, "done", Value::Object(throw_fn))?;

  define_data_property(&mut scope, iter, "result", Value::Object(iter_result))?;

  let return_this_id = rt.vm.register_native_call(return_this_native)?;
  let next_return_result_id = rt.vm.register_native_call(iterator_next_return_result_native)?;
  let return_set_closed_id = rt.vm.register_native_call(iterator_return_set_closed_native)?;

  let iter_name = scope.alloc_string("")?;
  let iter_method = scope.alloc_native_function(return_this_id, None, iter_name, 0)?;
  scope.push_root(Value::Object(iter_method))?;
  let next_name = scope.alloc_string("")?;
  let next = scope.alloc_native_function(next_return_result_id, None, next_name, 0)?;
  scope.push_root(Value::Object(next))?;
  let return_name = scope.alloc_string("")?;
  let return_ = scope.alloc_native_function(return_set_closed_id, None, return_name, 0)?;
  scope.push_root(Value::Object(return_))?;

  let iterator_key = PropertyKey::from_symbol(intr.well_known_symbols().iterator);
  scope.define_property(
    iter,
    iterator_key,
    PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::Object(iter_method),
        writable: true,
      },
    },
  )?;
  define_data_property(&mut scope, iter, "next", Value::Object(next))?;
  define_data_property(&mut scope, iter, "return", Value::Object(return_))?;

  let args = [Value::Object(iter)];
  let _err = rt
    .vm
    .call_without_host(&mut scope, Value::Object(from_entries), Value::Object(object), &args)
    .unwrap_err();

  require_closed_flag(&mut scope, iter, false)?;
  Ok(())
}

#[test]
fn object_from_entries_iterator_not_closed_for_next_returning_null() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;
  let intr = *rt.realm.intrinsics();
  let object = intr.object_constructor();

  let mut scope = rt.heap.scope();

  let from_entries = get_own_data_property(&mut scope, object, "fromEntries")?.unwrap();
  let Value::Object(from_entries) = from_entries else {
    return Err(VmError::Unimplemented("Object.fromEntries is not a function object"));
  };

  let iter = scope.alloc_object()?;
  scope.push_root(Value::Object(iter))?;
  define_data_property(&mut scope, iter, "closed", Value::Bool(false))?;

  let return_this_id = rt.vm.register_native_call(return_this_native)?;
  let next_return_null_id = rt.vm.register_native_call(iterator_next_return_null_native)?;
  let return_set_closed_id = rt.vm.register_native_call(iterator_return_set_closed_native)?;

  let iter_name = scope.alloc_string("")?;
  let iter_method = scope.alloc_native_function(return_this_id, None, iter_name, 0)?;
  scope.push_root(Value::Object(iter_method))?;
  let next_name = scope.alloc_string("")?;
  let next = scope.alloc_native_function(next_return_null_id, None, next_name, 0)?;
  scope.push_root(Value::Object(next))?;
  let return_name = scope.alloc_string("")?;
  let return_ = scope.alloc_native_function(return_set_closed_id, None, return_name, 0)?;
  scope.push_root(Value::Object(return_))?;

  let iterator_key = PropertyKey::from_symbol(intr.well_known_symbols().iterator);
  scope.define_property(
    iter,
    iterator_key,
    PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::Object(iter_method),
        writable: true,
      },
    },
  )?;
  define_data_property(&mut scope, iter, "next", Value::Object(next))?;
  define_data_property(&mut scope, iter, "return", Value::Object(return_))?;

  let args = [Value::Object(iter)];
  let _err = rt
    .vm
    .call_without_host(&mut scope, Value::Object(from_entries), Value::Object(object), &args)
    .unwrap_err();

  require_closed_flag(&mut scope, iter, false)?;
  Ok(())
}

#[test]
fn object_from_entries_iterator_closed_for_null_entry() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;
  let intr = *rt.realm.intrinsics();
  let object = intr.object_constructor();

  let mut scope = rt.heap.scope();

  let from_entries = get_own_data_property(&mut scope, object, "fromEntries")?.unwrap();
  let Value::Object(from_entries) = from_entries else {
    return Err(VmError::Unimplemented("Object.fromEntries is not a function object"));
  };

  let iter = scope.alloc_object()?;
  scope.push_root(Value::Object(iter))?;
  define_data_property(&mut scope, iter, "closed", Value::Bool(false))?;

  let iter_result = scope.alloc_object()?;
  scope.push_root(Value::Object(iter_result))?;
  define_data_property(&mut scope, iter_result, "done", Value::Bool(false))?;
  define_data_property(&mut scope, iter_result, "value", Value::Null)?;
  define_data_property(&mut scope, iter, "result", Value::Object(iter_result))?;

  let return_this_id = rt.vm.register_native_call(return_this_native)?;
  let next_return_result_id = rt.vm.register_native_call(iterator_next_return_result_native)?;
  let return_set_closed_id = rt.vm.register_native_call(iterator_return_set_closed_native)?;

  let iter_name = scope.alloc_string("")?;
  let iter_method = scope.alloc_native_function(return_this_id, None, iter_name, 0)?;
  scope.push_root(Value::Object(iter_method))?;
  let next_name = scope.alloc_string("")?;
  let next = scope.alloc_native_function(next_return_result_id, None, next_name, 0)?;
  scope.push_root(Value::Object(next))?;
  let return_name = scope.alloc_string("")?;
  let return_ = scope.alloc_native_function(return_set_closed_id, None, return_name, 0)?;
  scope.push_root(Value::Object(return_))?;

  let iterator_key = PropertyKey::from_symbol(intr.well_known_symbols().iterator);
  scope.define_property(
    iter,
    iterator_key,
    PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::Object(iter_method),
        writable: true,
      },
    },
  )?;
  define_data_property(&mut scope, iter, "next", Value::Object(next))?;
  define_data_property(&mut scope, iter, "return", Value::Object(return_))?;

  let args = [Value::Object(iter)];
  let _err = rt
    .vm
    .call_without_host(&mut scope, Value::Object(from_entries), Value::Object(object), &args)
    .unwrap_err();

  require_closed_flag(&mut scope, iter, true)?;
  Ok(())
}

#[test]
fn object_from_entries_iterator_closed_for_throwing_entry_key_accessor() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;
  let intr = *rt.realm.intrinsics();
  let object = intr.object_constructor();

  let mut scope = rt.heap.scope();

  let from_entries = get_own_data_property(&mut scope, object, "fromEntries")?.unwrap();
  let Value::Object(from_entries) = from_entries else {
    return Err(VmError::Unimplemented("Object.fromEntries is not a function object"));
  };

  let iter = scope.alloc_object()?;
  scope.push_root(Value::Object(iter))?;
  define_data_property(&mut scope, iter, "closed", Value::Bool(false))?;

  let entry = scope.alloc_object()?;
  scope.push_root(Value::Object(entry))?;
  let throw_id = rt.vm.register_native_call(throw_type_error_native)?;
  let throw_name = scope.alloc_string("")?;
  let throw_fn = scope.alloc_native_function(throw_id, None, throw_name, 0)?;
  scope.push_root(Value::Object(throw_fn))?;
  define_accessor_property(&mut scope, entry, "0", Value::Object(throw_fn))?;
  define_data_property(&mut scope, entry, "1", Value::Undefined)?;

  let iter_result = scope.alloc_object()?;
  scope.push_root(Value::Object(iter_result))?;
  define_data_property(&mut scope, iter_result, "done", Value::Bool(false))?;
  define_data_property(&mut scope, iter_result, "value", Value::Object(entry))?;
  define_data_property(&mut scope, iter, "result", Value::Object(iter_result))?;

  let return_this_id = rt.vm.register_native_call(return_this_native)?;
  let next_return_result_id = rt.vm.register_native_call(iterator_next_return_result_native)?;
  let return_set_closed_id = rt.vm.register_native_call(iterator_return_set_closed_native)?;

  let iter_name = scope.alloc_string("")?;
  let iter_method = scope.alloc_native_function(return_this_id, None, iter_name, 0)?;
  scope.push_root(Value::Object(iter_method))?;
  let next_name = scope.alloc_string("")?;
  let next = scope.alloc_native_function(next_return_result_id, None, next_name, 0)?;
  scope.push_root(Value::Object(next))?;
  let return_name = scope.alloc_string("")?;
  let return_ = scope.alloc_native_function(return_set_closed_id, None, return_name, 0)?;
  scope.push_root(Value::Object(return_))?;

  let iterator_key = PropertyKey::from_symbol(intr.well_known_symbols().iterator);
  scope.define_property(
    iter,
    iterator_key,
    PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::Object(iter_method),
        writable: true,
      },
    },
  )?;
  define_data_property(&mut scope, iter, "next", Value::Object(next))?;
  define_data_property(&mut scope, iter, "return", Value::Object(return_))?;

  let args = [Value::Object(iter)];
  let _err = rt
    .vm
    .call_without_host(&mut scope, Value::Object(from_entries), Value::Object(object), &args)
    .unwrap_err();

  require_closed_flag(&mut scope, iter, true)?;
  Ok(())
}

#[test]
fn object_from_entries_iterator_closed_for_throwing_entry_value_accessor() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;
  let intr = *rt.realm.intrinsics();
  let object = intr.object_constructor();

  let mut scope = rt.heap.scope();

  let from_entries = get_own_data_property(&mut scope, object, "fromEntries")?.unwrap();
  let Value::Object(from_entries) = from_entries else {
    return Err(VmError::Unimplemented("Object.fromEntries is not a function object"));
  };

  let iter = scope.alloc_object()?;
  scope.push_root(Value::Object(iter))?;
  define_data_property(&mut scope, iter, "closed", Value::Bool(false))?;

  let entry = scope.alloc_object()?;
  scope.push_root(Value::Object(entry))?;
  let key_s = scope.alloc_string("a")?;
  scope.push_root(Value::String(key_s))?;
  define_data_property(&mut scope, entry, "0", Value::String(key_s))?;

  let throw_id = rt.vm.register_native_call(throw_type_error_native)?;
  let throw_name = scope.alloc_string("")?;
  let throw_fn = scope.alloc_native_function(throw_id, None, throw_name, 0)?;
  scope.push_root(Value::Object(throw_fn))?;
  define_accessor_property(&mut scope, entry, "1", Value::Object(throw_fn))?;

  let iter_result = scope.alloc_object()?;
  scope.push_root(Value::Object(iter_result))?;
  define_data_property(&mut scope, iter_result, "done", Value::Bool(false))?;
  define_data_property(&mut scope, iter_result, "value", Value::Object(entry))?;
  define_data_property(&mut scope, iter, "result", Value::Object(iter_result))?;

  let return_this_id = rt.vm.register_native_call(return_this_native)?;
  let next_return_result_id = rt.vm.register_native_call(iterator_next_return_result_native)?;
  let return_set_closed_id = rt.vm.register_native_call(iterator_return_set_closed_native)?;

  let iter_name = scope.alloc_string("")?;
  let iter_method = scope.alloc_native_function(return_this_id, None, iter_name, 0)?;
  scope.push_root(Value::Object(iter_method))?;
  let next_name = scope.alloc_string("")?;
  let next = scope.alloc_native_function(next_return_result_id, None, next_name, 0)?;
  scope.push_root(Value::Object(next))?;
  let return_name = scope.alloc_string("")?;
  let return_ = scope.alloc_native_function(return_set_closed_id, None, return_name, 0)?;
  scope.push_root(Value::Object(return_))?;

  let iterator_key = PropertyKey::from_symbol(intr.well_known_symbols().iterator);
  scope.define_property(
    iter,
    iterator_key,
    PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::Object(iter_method),
        writable: true,
      },
    },
  )?;
  define_data_property(&mut scope, iter, "next", Value::Object(next))?;
  define_data_property(&mut scope, iter, "return", Value::Object(return_))?;

  let args = [Value::Object(iter)];
  let _err = rt
    .vm
    .call_without_host(&mut scope, Value::Object(from_entries), Value::Object(object), &args)
    .unwrap_err();

  require_closed_flag(&mut scope, iter, true)?;
  Ok(())
}

#[test]
fn object_from_entries_iterator_closed_for_throwing_entry_key_tostring() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;
  let intr = *rt.realm.intrinsics();
  let object = intr.object_constructor();

  let mut scope = rt.heap.scope();

  let from_entries = get_own_data_property(&mut scope, object, "fromEntries")?.unwrap();
  let Value::Object(from_entries) = from_entries else {
    return Err(VmError::Unimplemented("Object.fromEntries is not a function object"));
  };

  let iter = scope.alloc_object()?;
  scope.push_root(Value::Object(iter))?;
  define_data_property(&mut scope, iter, "closed", Value::Bool(false))?;

  let key_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(key_obj))?;
  let throw_id = rt.vm.register_native_call(throw_type_error_native)?;
  let throw_name = scope.alloc_string("")?;
  let throw_fn = scope.alloc_native_function(throw_id, None, throw_name, 0)?;
  scope.push_root(Value::Object(throw_fn))?;
  define_data_property(&mut scope, key_obj, "toString", Value::Object(throw_fn))?;

  let entry = scope.alloc_object()?;
  scope.push_root(Value::Object(entry))?;
  define_data_property(&mut scope, entry, "0", Value::Object(key_obj))?;
  define_data_property(&mut scope, entry, "1", Value::Undefined)?;

  let iter_result = scope.alloc_object()?;
  scope.push_root(Value::Object(iter_result))?;
  define_data_property(&mut scope, iter_result, "done", Value::Bool(false))?;
  define_data_property(&mut scope, iter_result, "value", Value::Object(entry))?;
  define_data_property(&mut scope, iter, "result", Value::Object(iter_result))?;

  let return_this_id = rt.vm.register_native_call(return_this_native)?;
  let next_return_result_id = rt.vm.register_native_call(iterator_next_return_result_native)?;
  let return_set_closed_id = rt.vm.register_native_call(iterator_return_set_closed_native)?;

  let iter_name = scope.alloc_string("")?;
  let iter_method = scope.alloc_native_function(return_this_id, None, iter_name, 0)?;
  scope.push_root(Value::Object(iter_method))?;
  let next_name = scope.alloc_string("")?;
  let next = scope.alloc_native_function(next_return_result_id, None, next_name, 0)?;
  scope.push_root(Value::Object(next))?;
  let return_name = scope.alloc_string("")?;
  let return_ = scope.alloc_native_function(return_set_closed_id, None, return_name, 0)?;
  scope.push_root(Value::Object(return_))?;

  let iterator_key = PropertyKey::from_symbol(intr.well_known_symbols().iterator);
  scope.define_property(
    iter,
    iterator_key,
    PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::Object(iter_method),
        writable: true,
      },
    },
  )?;
  define_data_property(&mut scope, iter, "next", Value::Object(next))?;
  define_data_property(&mut scope, iter, "return", Value::Object(return_))?;

  let args = [Value::Object(iter)];
  let _err = rt
    .vm
    .call_without_host(&mut scope, Value::Object(from_entries), Value::Object(object), &args)
    .unwrap_err();

  require_closed_flag(&mut scope, iter, true)?;
  Ok(())
}

#[test]
fn object_from_entries_iterator_close_error_does_not_override_thrown_entry_error() -> Result<(), VmError> {
  let mut rt = TestRealm::new()?;
  let intr = *rt.realm.intrinsics();
  let object = intr.object_constructor();

  let mut scope = rt.heap.scope();

  let from_entries = get_own_data_property(&mut scope, object, "fromEntries")?.unwrap();
  let Value::Object(from_entries) = from_entries else {
    return Err(VmError::Unimplemented("Object.fromEntries is not a function object"));
  };

  let iter = scope.alloc_object()?;
  scope.push_root(Value::Object(iter))?;
  define_data_property(&mut scope, iter, "closed", Value::Bool(false))?;

  // Entry object where accessing key "0" throws a specific object value.
  let thrown_obj = scope.alloc_object()?;
  scope.push_root(Value::Object(thrown_obj))?;

  let throw_slot0_id = rt.vm.register_native_call(throw_slot0_native)?;
  let throw_name = scope.alloc_string("")?;
  let throw_fn = scope.alloc_native_function_with_slots(
    throw_slot0_id,
    None,
    throw_name,
    0,
    &[Value::Object(thrown_obj)],
  )?;
  scope.push_root(Value::Object(throw_fn))?;

  let entry = scope.alloc_object()?;
  scope.push_root(Value::Object(entry))?;
  define_accessor_property(&mut scope, entry, "0", Value::Object(throw_fn))?;
  define_data_property(&mut scope, entry, "1", Value::Undefined)?;

  let iter_result = scope.alloc_object()?;
  scope.push_root(Value::Object(iter_result))?;
  define_data_property(&mut scope, iter_result, "done", Value::Bool(false))?;
  define_data_property(&mut scope, iter_result, "value", Value::Object(entry))?;
  define_data_property(&mut scope, iter, "result", Value::Object(iter_result))?;

  let return_this_id = rt.vm.register_native_call(return_this_native)?;
  let next_return_result_id = rt.vm.register_native_call(iterator_next_return_result_native)?;
  let return_set_closed_undefined_id =
    rt.vm.register_native_call(iterator_return_set_closed_returns_undefined_native)?;

  let iter_name = scope.alloc_string("")?;
  let iter_method = scope.alloc_native_function(return_this_id, None, iter_name, 0)?;
  scope.push_root(Value::Object(iter_method))?;
  let next_name = scope.alloc_string("")?;
  let next = scope.alloc_native_function(next_return_result_id, None, next_name, 0)?;
  scope.push_root(Value::Object(next))?;
  let return_name = scope.alloc_string("")?;
  let return_ = scope.alloc_native_function(return_set_closed_undefined_id, None, return_name, 0)?;
  scope.push_root(Value::Object(return_))?;

  let iterator_key = PropertyKey::from_symbol(intr.well_known_symbols().iterator);
  scope.define_property(
    iter,
    iterator_key,
    PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::Object(iter_method),
        writable: true,
      },
    },
  )?;
  define_data_property(&mut scope, iter, "next", Value::Object(next))?;
  define_data_property(&mut scope, iter, "return", Value::Object(return_))?;

  let args = [Value::Object(iter)];
  let err = rt
    .vm
    .call_without_host(&mut scope, Value::Object(from_entries), Value::Object(object), &args)
    .unwrap_err();

  assert_eq!(err.thrown_value(), Some(Value::Object(thrown_obj)));
  require_closed_flag(&mut scope, iter, true)?;
  Ok(())
}

#[test]
fn object_from_entries_iterator_close_return_throw_overrides_throw_completion() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let value = rt.exec_script(
    r#"
      (function () {
        var returnCalled = false;
        var iterable = {};

        iterable[Symbol.iterator] = function() {
          var done = false;
          return {
            next: function() {
              if (!done) {
                done = true;
                // Non-object entry triggers the entry-processing TypeError inside Object.fromEntries,
                 // then `IteratorClose` is performed. Per ECMA-262 `IteratorClose`, errors thrown while
                 // getting/calling `iterator.return` override the incoming completion (even when that
                 // incoming completion is itself a throw completion).
                return { done: false, value: 1 };
              }
              return { done: true };
            },
            return: function() {
              returnCalled = true;
              throw "return";
            },
          };
        };

        try {
          Object.fromEntries(iterable);
          return "no throw";
        } catch (e) {
          return String(returnCalled) + "|" + ((e && e.name) || e);
        }
      })()
    "#,
  )?;

  assert_eq!(
    as_utf8_lossy(&rt, value),
    "true|return"
  );
  Ok(())
}
