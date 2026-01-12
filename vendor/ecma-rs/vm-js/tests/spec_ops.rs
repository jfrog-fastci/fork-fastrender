use vm_js::{
  get_prototype_from_constructor, species_constructor, Heap, HeapLimits, JsRuntime, PropertyDescriptor,
  PropertyKey, PropertyKind, Realm, Scope, Value, Vm, VmError, VmHost, VmHostHooks, VmOptions,
};

fn data_desc(value: Value) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable: true,
    configurable: true,
    kind: PropertyKind::Data {
      value,
      writable: true,
    },
  }
}

fn executor_noop(
  _vm: &mut Vm,
  _scope: &mut Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: vm_js::GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  Ok(Value::Undefined)
}

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn get_prototype_from_constructor_uses_prototype_property_when_object() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

  let mut scope = heap.scope();
  let ctor = scope.alloc_object()?;
  let explicit_proto = scope.alloc_object()?;
  let default_proto = scope.alloc_object()?;

  let key = PropertyKey::from_string(scope.alloc_string("prototype")?);
  scope.define_property(ctor, key, data_desc(Value::Object(explicit_proto)))?;

  let proto = get_prototype_from_constructor(&mut vm, &mut scope, Value::Object(ctor), default_proto)?;
  assert_eq!(proto, explicit_proto);
  Ok(())
}

#[test]
fn get_prototype_from_constructor_falls_back_when_not_object() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

  let mut scope = heap.scope();
  let ctor = scope.alloc_object()?;
  let default_proto = scope.alloc_object()?;

  let key = PropertyKey::from_string(scope.alloc_string("prototype")?);
  scope.define_property(ctor, key, data_desc(Value::Number(123.0)))?;

  let proto = get_prototype_from_constructor(&mut vm, &mut scope, Value::Object(ctor), default_proto)?;
  assert_eq!(proto, default_proto);
  Ok(())
}

#[test]
fn promise_constructor_sets_instance_prototype_from_new_target() -> Result<(), VmError> {
  let mut vm = Vm::new(VmOptions::default());
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let promise = realm.intrinsics().promise();

  let promise_instance = {
    let mut scope = heap.scope();
    let call_id = vm.register_native_call(executor_noop)?;
    let executor_name = scope.alloc_string("executor")?;
    let executor = scope.alloc_native_function(call_id, None, executor_name, 0)?;

    let overridden_proto = scope.alloc_object()?;
    let new_target = scope.alloc_object()?;
    let key = PropertyKey::from_string(scope.alloc_string("prototype")?);
    scope.define_property(new_target, key, data_desc(Value::Object(overridden_proto)))?;

    let value = vm.construct_without_host(
      &mut scope,
      Value::Object(promise),
      &[Value::Object(executor)],
      Value::Object(new_target),
    )?;
    let Value::Object(obj) = value else {
      panic!("expected Promise object, got {value:?}");
    };

    assert_eq!(scope.heap().object_prototype(obj)?, Some(overridden_proto));
    obj
  };

  // Ensure the object stays alive until after we tear down the realm roots.
  assert!(heap.is_valid_object(promise_instance));

  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn species_constructor_observes_proxy_get_traps() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var log = [];
      var ctor = new Proxy(function(){}, {
        get: function (t, k, r) {
          log.push("ctor:" + String(k));
          if (k === Symbol.species) return undefined;
        },
      });
      var p = new Proxy({}, {
        get: function (t, k, r) {
          log.push("obj:" + String(k));
          if (k === "constructor") return ctor;
        },
      });
    "#,
  )?;

  let p = match rt.exec_script("p")? {
    Value::Object(o) => o,
    other => panic!("expected Proxy object, got {other:?}"),
  };
  let default_ctor = Value::Object(rt.realm().intrinsics().object_constructor());

  let result = {
    let (vm, _realm, heap) = rt.vm_realm_and_heap_mut();
    let mut scope = heap.scope();
    species_constructor(vm, &mut scope, p, default_ctor)?
  };
  assert_eq!(result, default_ctor);

  let log_str = match rt.exec_script("log.join(',')")? {
    Value::String(s) => rt.heap.get_string(s)?.to_utf8_lossy(),
    other => panic!("expected string log output, got {other:?}"),
  };
  assert!(
    log_str.contains("obj:constructor"),
    "expected Proxy get trap to observe constructor property, got log {log_str:?}"
  );
  assert!(
    log_str.contains("Symbol.species"),
    "expected Proxy get trap to observe @@species property, got log {log_str:?}"
  );
  Ok(())
}
