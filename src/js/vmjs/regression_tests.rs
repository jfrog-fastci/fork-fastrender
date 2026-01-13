use vm_js::{
  Heap, HeapLimits, JsRuntime, PromiseHandle, PromiseRejectionHandleAction,
  PromiseRejectionTracker, PropertyKey, Value, Vm, VmOptions,
};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn assert_exec_throws_type_error(rt: &mut JsRuntime, script: &str) {
  let err = rt.exec_script(script).unwrap_err();
  let Some(thrown) = err.thrown_value() else {
    panic!("expected a thrown JS exception for {script:?}, got {err:?}");
  };
  let Value::Object(obj) = thrown else {
    panic!("expected a thrown object for {script:?}, got {thrown:?}");
  };

  // Root the thrown value across any heap allocations needed to probe it.
  let root = rt.heap_mut().add_root(thrown).unwrap();

  let name_key = {
    let mut scope = rt.heap_mut().scope();
    PropertyKey::from_string(scope.alloc_string("name").unwrap())
  };
  let name_value = rt
    .heap()
    .object_get_own_data_property_value(obj, &name_key)
    .unwrap()
    .unwrap();
  let Value::String(name) = name_value else {
    panic!("expected TypeError.name to be a string, got {name_value:?}");
  };
  let name = rt.heap().get_string(name).unwrap().to_utf8_lossy();
  assert_eq!(name, "TypeError");

  rt.heap_mut().remove_root(root);
}

#[test]
fn plus_operator_concatenates_when_either_side_is_string() {
  let mut rt = new_runtime();

  let value = rt.exec_script(r#""1" + 2 === "12""#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(r#"1 + "2" === "12""#).unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn numeric_operators_use_tonumber_for_strings() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#""5" - 2 === 3"#).unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn abstract_equality_matches_ecmascript_primitives() {
  let mut rt = new_runtime();

  let value = rt.exec_script(r#"null == undefined"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(r#"false == 0"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(r#"true == 1"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(r#""0" == 0"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  // Extra primitive coverage.
  let value = rt.exec_script(r#""0" == false"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(r#"'' == 0"#).unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn tonumber_parses_whitespace_radixes_and_infinity() {
  let mut rt = new_runtime();

  let value = rt.exec_script(r#"+'  1  ' === 1"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(r#"+'0x10' === 16"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  // Signed hex/binary/octal forms are *not* valid `StringToNumber` inputs.
  // (e.g. `Number("-0x10")` is `NaN`; use `parseInt` for signed radix parsing).
  let value = rt.exec_script(r#"+'+0x10' !== +'+0x10'"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(r#"+'-0x10' !== +'-0x10'"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(r#"+'0b10' === 2"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(r#"+'+0b10' !== +'+0b10'"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(r#"+'-0b10' !== +'-0b10'"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(r#"+'0o10' === 8"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(r#"+'+0o10' !== +'+0o10'"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(r#"+'-0o10' !== +'-0o10'"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  // Infinity parsing is case-sensitive in ECMAScript.
  let value = rt.exec_script(r#"+'Infinity' === 1e999"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(r#"+'-Infinity' === -1e999"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  // Ensure we don't accept Rust's "inf"/"infinity" shorthands.
  let value = rt.exec_script(r#"+'inf' !== +'inf'"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(r#"+'' === 0"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(r#"+'   ' === 0"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  // Empty radix prefixes parse to NaN.
  let value = rt.exec_script(r#"+'0x' !== +'0x'"#).unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn objects_use_toprimitive_for_addition_and_equality() {
  let mut rt = new_runtime();

  // Ordinary objects stringify to "[object Object]" when coerced.
  let value = rt
    .exec_script(r#"({}) + 'x' === '[object Object]x'"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt.exec_script(r#"({}) == '[object Object]'"#).unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn abstract_equality_null_is_not_equal_to_zero() {
  let mut rt = new_runtime();

  let value = rt.exec_script(r#"null == 0"#).unwrap();
  assert_eq!(value, Value::Bool(false));

  let value = rt.exec_script(r#"undefined == 0"#).unwrap();
  assert_eq!(value, Value::Bool(false));
}

#[test]
fn symbol_coercions_throw_typeerror() {
  let mut rt = new_runtime();

  assert_exec_throws_type_error(&mut rt, r#"+Symbol('x')"#);

  // String concatenation uses `ToString`, which throws for Symbols.
  assert_exec_throws_type_error(&mut rt, r#"'' + Symbol('x')"#);

  assert_exec_throws_type_error(&mut rt, r#"Symbol('x') + ''"#);

  // But equality just returns false (no coercion to string/number).
  let value = rt.exec_script(r#"Symbol('x') == 'x'"#).unwrap();
  assert_eq!(value, Value::Bool(false));
}

#[test]
fn string_pad_start_end() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        "a".padStart(3, "0") === "00a"
          && "a".padEnd(3, "0") === "a00"
          && "a".padStart(3) === "  a"
          && "a".padEnd(3) === "a  "
          && "abc".padStart(6, "01") === "010abc"
          && "abc".padEnd(6, "01") === "abc010"
          && "a".padStart(3, "") === "a"
          && "a".padEnd(3, "") === "a"
          && "abcd".padStart(2, "0") === "abcd"
          && "abcd".padEnd(2, "0") === "abcd"
          && "abc".padStart() === "abc"
          && "abc".padEnd() === "abc"
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn promise_rejection_tracker_api_smoke() {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

  let promise: PromiseHandle;
  {
    let mut scope = heap.scope();
    let obj = scope.alloc_object().unwrap();
    promise = PromiseHandle::from(obj);
  }

  let mut tracker = PromiseRejectionTracker::new();
  tracker.on_reject(&mut heap, promise);

  let batch = tracker.drain_about_to_be_notified(&mut heap);
  assert_eq!(batch.promises(), &[promise]);
  batch.teardown(&mut heap);

  tracker.after_unhandledrejection_dispatch(promise, false);
  assert_eq!(
    tracker.on_handle(&mut heap, promise),
    PromiseRejectionHandleAction::QueueRejectionHandled { promise }
  );
}

mod define_own_property_smoke {
  use vm_js::{
    Heap, HeapLimits, PropertyDescriptor, PropertyDescriptorPatch, PropertyKey, PropertyKind,
    Value, VmError,
  };

  #[test]
  fn reject_changing_enumerable_on_non_configurable_property() -> Result<(), VmError> {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

    let (obj, key) = {
      let mut scope = heap.scope();
      let obj = scope.alloc_object()?;
      let key = PropertyKey::from_string(scope.alloc_string("x")?);
      scope.define_property(
        obj,
        key,
        PropertyDescriptor {
          enumerable: true,
          configurable: false,
          kind: PropertyKind::Data {
            value: Value::Undefined,
            writable: true,
          },
        },
      )?;
      (obj, key)
    };

    assert!(!heap.define_own_property(
      obj,
      key,
      PropertyDescriptorPatch {
        enumerable: Some(false),
        ..Default::default()
      },
    )?);

    Ok(())
  }

  #[test]
  fn empty_patch_creates_default_data_descriptor() -> Result<(), VmError> {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

    let (obj, key) = {
      let mut scope = heap.scope();
      let obj = scope.alloc_object()?;
      let key = PropertyKey::from_string(scope.alloc_string("x")?);
      (obj, key)
    };

    assert!(heap.define_own_property(obj, key, PropertyDescriptorPatch::default())?);

    let desc = heap
      .object_get_own_property(obj, &key)?
      .expect("property should exist");

    assert!(!desc.enumerable);
    assert!(!desc.configurable);
    match desc.kind {
      PropertyKind::Data { value, writable } => {
        assert!(matches!(value, Value::Undefined));
        assert!(!writable);
      }
      PropertyKind::Accessor { .. } => panic!("expected a data property"),
    }

    Ok(())
  }

  #[test]
  fn reject_value_changes_on_non_writable_non_configurable_data_property() -> Result<(), VmError> {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

    let (obj, key) = {
      let mut scope = heap.scope();
      let obj = scope.alloc_object()?;
      let key = PropertyKey::from_string(scope.alloc_string("x")?);
      scope.define_property(
        obj,
        key,
        PropertyDescriptor {
          enumerable: true,
          configurable: false,
          kind: PropertyKind::Data {
            value: Value::Number(1.0),
            writable: false,
          },
        },
      )?;
      (obj, key)
    };

    // Changing the value should be rejected.
    assert!(!heap.define_own_property(
      obj,
      key,
      PropertyDescriptorPatch {
        value: Some(Value::Number(2.0)),
        ..Default::default()
      },
    )?);

    // Changing writable from false -> true should be rejected.
    assert!(!heap.define_own_property(
      obj,
      key,
      PropertyDescriptorPatch {
        writable: Some(true),
        ..Default::default()
      },
    )?);

    // Re-defining with the same value is allowed (SameValue).
    assert!(heap.define_own_property(
      obj,
      key,
      PropertyDescriptorPatch {
        value: Some(Value::Number(1.0)),
        ..Default::default()
      },
    )?);

    Ok(())
  }

  #[test]
  fn reject_getter_changes_on_non_configurable_accessor_property() -> Result<(), VmError> {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

    let (obj, key, get1, get2) = {
      let mut scope = heap.scope();

      let get1 = scope.alloc_object()?;
      let get2 = scope.alloc_object()?;

      let obj = scope.alloc_object()?;
      let key = PropertyKey::from_string(scope.alloc_string("x")?);
      scope.define_property(
        obj,
        key,
        PropertyDescriptor {
          enumerable: true,
          configurable: false,
          kind: PropertyKind::Accessor {
            get: Value::Object(get1),
            set: Value::Undefined,
          },
        },
      )?;
      (obj, key, get1, get2)
    };

    assert!(!heap.define_own_property(
      obj,
      key,
      PropertyDescriptorPatch {
        get: Some(Value::Object(get2)),
        ..Default::default()
      },
    )?);

    // Re-defining with the same getter is allowed (SameValue).
    assert!(heap.define_own_property(
      obj,
      key,
      PropertyDescriptorPatch {
        get: Some(Value::Object(get1)),
        ..Default::default()
      },
    )?);

    Ok(())
  }

  #[test]
  fn respects_non_extensible_object() -> Result<(), VmError> {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

    let (obj, key) = {
      let mut scope = heap.scope();
      let obj = scope.alloc_object()?;
      scope.object_prevent_extensions(obj)?;
      let key = PropertyKey::from_string(scope.alloc_string("x")?);
      (obj, key)
    };

    assert!(!heap.define_own_property(obj, key, PropertyDescriptorPatch::default())?);
    assert!(heap.object_get_own_property(obj, &key)?.is_none());
    Ok(())
  }
}

mod function_call_apply_bind_smoke {
  use vm_js::{
    GcObject, Heap, HeapLimits, PropertyDescriptor, PropertyKey, PropertyKind, Realm, Scope, Value,
    Vm, VmError, VmHost, VmHostHooks, VmOptions,
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

  fn reflect_native(
    _vm: &mut Vm,
    _scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    _hooks: &mut dyn VmHostHooks,
    _callee: GcObject,
    this: Value,
    args: &[Value],
  ) -> Result<Value, VmError> {
    if args.is_empty() {
      Ok(this)
    } else {
      Ok(args[0])
    }
  }

  fn define_enumerable_data_property(
    scope: &mut Scope<'_>,
    obj: GcObject,
    name: &str,
    value: Value,
  ) -> Result<(), VmError> {
    let mut scope = scope.reborrow();
    scope.push_root(Value::Object(obj))?;
    scope.push_root(value)?;
    let key_s = scope.alloc_string(name)?;
    scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);
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

  #[test]
  fn function_call_apply_bind_smoke() -> Result<(), VmError> {
    let mut rt = TestRealm::new()?;
    let mut scope = rt.heap.scope();

    // Create a host-native function with Function.prototype in its prototype chain.
    let reflect_id = rt.vm.register_native_call(reflect_native)?;
    let name = scope.alloc_string("reflect")?;
    scope.push_root(Value::String(name))?;
    let reflect_fn = scope.alloc_native_function(reflect_id, None, name, 0)?;
    // `alloc_native_function` does not wire up `[[Prototype]]` automatically; mirror the embedder
    // behavior in `src/js/*` by linking native functions to `Function.prototype`.
    scope
      .heap_mut()
      .object_set_prototype(reflect_fn, Some(rt.realm.intrinsics().function_prototype()))?;
    scope.push_root(Value::Object(reflect_fn))?;

    let this_obj = scope.alloc_object()?;
    scope.push_root(Value::Object(this_obj))?;

    // --- Function.prototype.call ---
    let call_key_s = scope.alloc_string("call")?;
    scope.push_root(Value::String(call_key_s))?;
    let call_key = PropertyKey::from_string(call_key_s);
    let call = rt.vm.get(&mut scope, reflect_fn, call_key)?;
    let Value::Object(call) = call else {
      panic!("expected Function.prototype.call to be callable");
    };
    let result = rt.vm.call_without_host(
      &mut scope,
      Value::Object(call),
      Value::Object(reflect_fn),
      &[Value::Object(this_obj)],
    )?;
    assert_eq!(result, Value::Object(this_obj));

    // --- Function.prototype.apply ---
    //
    // `apply`/`bind` are optional here: this smoke test is primarily intended to ensure that native
    // host-created functions can reach `Function.prototype.call` via the `[[Prototype]]` chain.
    let apply_key_s = scope.alloc_string("apply")?;
    scope.push_root(Value::String(apply_key_s))?;
    let apply_key = PropertyKey::from_string(apply_key_s);
    let apply = rt.vm.get(&mut scope, reflect_fn, apply_key)?;
    match apply {
      Value::Undefined => {}
      Value::Object(apply) => {
        let args_arr = scope.alloc_array(1)?;
        scope.push_root(Value::Object(args_arr))?;
        define_enumerable_data_property(&mut scope, args_arr, "0", Value::Number(7.0))?;

        let result = rt.vm.call_without_host(
          &mut scope,
          Value::Object(apply),
          Value::Object(reflect_fn),
          &[Value::Object(this_obj), Value::Object(args_arr)],
        )?;
        assert_eq!(result, Value::Number(7.0));
      }
      other => {
        panic!("expected Function.prototype.apply to be callable or undefined, got {other:?}")
      }
    }

    // --- Function.prototype.bind ---
    let bind_key_s = scope.alloc_string("bind")?;
    scope.push_root(Value::String(bind_key_s))?;
    let bind_key = PropertyKey::from_string(bind_key_s);
    let bind = rt.vm.get(&mut scope, reflect_fn, bind_key)?;
    match bind {
      Value::Undefined => {}
      Value::Object(bind) => {
        // Bind only `thisArg`.
        let bound_this = rt.vm.call_without_host(
          &mut scope,
          Value::Object(bind),
          Value::Object(reflect_fn),
          &[Value::Object(this_obj)],
        )?;
        let Value::Object(bound_this) = bound_this else {
          panic!("expected bind() to return a function object");
        };
        scope.push_root(Value::Object(bound_this))?;

        let result =
          rt.vm
            .call_without_host(&mut scope, Value::Object(bound_this), Value::Undefined, &[])?;
        assert_eq!(result, Value::Object(this_obj));

        // Bind `thisArg` + a leading argument.
        let bound = rt.vm.call_without_host(
          &mut scope,
          Value::Object(bind),
          Value::Object(reflect_fn),
          &[Value::Object(this_obj), Value::Number(5.0)],
        )?;
        let Value::Object(bound_fn) = bound else {
          panic!("expected bind() to return a function object");
        };
        scope.push_root(Value::Object(bound_fn))?;

        // Bound function prepends bound args.
        let result = rt.vm.call_without_host(
          &mut scope,
          Value::Object(bound_fn),
          Value::Undefined,
          &[Value::Number(6.0)],
        )?;
        assert_eq!(result, Value::Number(5.0));
      }
      other => {
        panic!("expected Function.prototype.bind to be callable or undefined, got {other:?}")
      }
    }

    Ok(())
  }
}

mod vmjs_module_loading_smoke {
  use std::collections::VecDeque;
  use vm_js::{
    load_requested_modules, HostDefined, Job, JsString, ModuleGraph, ModuleId, ModuleLoadPayload,
    ModuleReferrer, ModuleRequest, ModuleStatus, PromiseState, Realm, SourceTextModuleRecord,
    Value, Vm, VmError, VmHostHooks, VmOptions,
  };

  struct TestRealm {
    vm: Vm,
    heap: vm_js::Heap,
    realm: Realm,
  }

  impl TestRealm {
    fn new() -> Result<Self, VmError> {
      let mut vm = Vm::new(VmOptions::default());
      let mut heap = vm_js::Heap::new(vm_js::HeapLimits::new(1024 * 1024, 1024 * 1024));
      let realm = Realm::new(&mut vm, &mut heap)?;
      Ok(Self { vm, heap, realm })
    }
  }

  impl Drop for TestRealm {
    fn drop(&mut self) {
      self.realm.teardown(&mut self.heap);
    }
  }

  struct TestHost {
    last_loaded: Option<ModuleId>,
    jobs: VecDeque<Job>,
  }

  impl TestHost {
    fn new() -> Self {
      Self {
        last_loaded: None,
        jobs: VecDeque::new(),
      }
    }
  }

  impl VmHostHooks for TestHost {
    fn host_enqueue_promise_job(&mut self, job: Job, _realm: Option<vm_js::RealmId>) {
      self.jobs.push_back(job);
    }

    fn host_load_imported_module(
      &mut self,
      vm: &mut Vm,
      scope: &mut vm_js::Scope<'_>,
      modules: &mut ModuleGraph,
      referrer: ModuleReferrer,
      module_request: ModuleRequest,
      _host_defined: HostDefined,
      payload: ModuleLoadPayload,
    ) -> Result<(), VmError> {
      // Synchronously complete the host hook by creating a trivial cyclic module and reporting it as
      // loaded. This re-enters the loader via `finish_loading_imported_module`, matching the spec
      // allowance for synchronous completion.
      let loaded = modules.add_module(SourceTextModuleRecord::default())?;
      self.last_loaded = Some(loaded);
      vm.finish_loading_imported_module(
        scope,
        modules,
        self,
        referrer,
        module_request,
        payload,
        Ok(loaded),
      )
    }
  }

  #[test]
  fn module_loading_caches_loaded_modules_and_resolves_promise() -> Result<(), VmError> {
    let mut rt = TestRealm::new()?;
    let mut scope = rt.heap.scope();
    let mut modules = ModuleGraph::default();
    let mut host = TestHost::new();

    let request = ModuleRequest::new(JsString::from_str("dep.js").unwrap(), vec![]);
    let mut referrer_record = SourceTextModuleRecord::default();
    referrer_record.requested_modules.push(request.clone());
    let referrer = modules.add_module(referrer_record)?;

    let promise = load_requested_modules(
      &mut rt.vm,
      &mut scope,
      &mut modules,
      &mut host,
      referrer,
      HostDefined::default(),
    )?;
    scope.push_root(promise)?;
    let Value::Object(promise) = promise else {
      panic!("expected module loading to return a Promise object");
    };
    assert_eq!(
      scope.heap().promise_state(promise)?,
      PromiseState::Fulfilled
    );

    let loaded = host
      .last_loaded
      .expect("host should have been invoked for the requested module");

    let record = modules
      .get_module(referrer)
      .expect("referrer module should exist");
    assert_eq!(record.status, ModuleStatus::Unlinked);
    assert_eq!(record.loaded_modules.len(), 1);
    assert!(record.loaded_modules[0].request.spec_equal(&request));
    assert_eq!(record.loaded_modules[0].module, loaded);

    let loaded_record = modules
      .get_module(loaded)
      .expect("loaded module should exist");
    assert_eq!(loaded_record.status, ModuleStatus::Unlinked);

    Ok(())
  }

  #[derive(Clone)]
  struct PendingLoad {
    referrer: ModuleReferrer,
    request: ModuleRequest,
    payload: ModuleLoadPayload,
  }

  #[derive(Default)]
  struct PendingHost {
    pending: Vec<PendingLoad>,
    jobs: VecDeque<Job>,
  }

  impl VmHostHooks for PendingHost {
    fn host_enqueue_promise_job(&mut self, job: Job, _realm: Option<vm_js::RealmId>) {
      self.jobs.push_back(job);
    }

    fn host_load_imported_module(
      &mut self,
      _vm: &mut Vm,
      _scope: &mut vm_js::Scope<'_>,
      _modules: &mut ModuleGraph,
      referrer: ModuleReferrer,
      module_request: ModuleRequest,
      _host_defined: HostDefined,
      payload: ModuleLoadPayload,
    ) -> Result<(), VmError> {
      self.pending.push(PendingLoad {
        referrer,
        request: module_request,
        payload,
      });
      Ok(())
    }
  }

  #[test]
  fn module_loading_rejects_duplicate_loaded_module_mismatch() -> Result<(), VmError> {
    let mut rt = TestRealm::new()?;
    let mut scope = rt.heap.scope();
    let mut modules = ModuleGraph::default();
    let module1 = modules.add_module(SourceTextModuleRecord::default())?;
    let module2 = modules.add_module(SourceTextModuleRecord::default())?;

    let request_dup = ModuleRequest::new(JsString::from_str("dup.js").unwrap(), vec![]);
    let mut referrer_record = SourceTextModuleRecord::default();
    // Intentionally create duplicate entries in `[[RequestedModules]]` so the loader invokes the host
    // hook twice and exercises `finish_loading_imported_module`'s caching/mismatch logic.
    referrer_record.requested_modules = vec![request_dup.clone(), request_dup.clone()];
    let referrer_module = modules.add_module(referrer_record)?;

    let mut host = PendingHost::default();

    let promise = load_requested_modules(
      &mut rt.vm,
      &mut scope,
      &mut modules,
      &mut host,
      referrer_module,
      HostDefined::default(),
    )?;
    scope.push_root(promise)?;
    let Value::Object(promise) = promise else {
      panic!("expected module loading to return a Promise object");
    };
    assert_eq!(scope.heap().promise_state(promise)?, PromiseState::Pending);

    // Two modules are requested and none have completed yet.
    assert_eq!(host.pending.len(), 2);

    let PendingLoad {
      referrer: load_referrer,
      request,
      payload,
    } = host.pending[0].clone();
    assert!(request.spec_equal(&request_dup));

    let PendingLoad {
      referrer: load_referrer2,
      request: request2,
      payload: payload2,
    } = host.pending[1].clone();
    assert!(request2.spec_equal(&request_dup));

    // Complete the first load.
    rt.vm.finish_loading_imported_module(
      &mut scope,
      &mut modules,
      &mut host,
      load_referrer,
      request.clone(),
      payload.clone(),
      Ok(module1),
    )?;
    assert_eq!(scope.heap().promise_state(promise)?, PromiseState::Pending);

    // A second completion for the same request with a different module id should be treated as an
    // invariant violation and reject the module-graph-loading promise.
    rt.vm.finish_loading_imported_module(
      &mut scope,
      &mut modules,
      &mut host,
      load_referrer2,
      request2,
      payload2,
      Ok(module2),
    )?;

    assert_eq!(scope.heap().promise_state(promise)?, PromiseState::Rejected);

    let record = modules
      .get_module(referrer_module)
      .expect("referrer module should exist");
    assert_eq!(record.loaded_modules.len(), 1);
    assert_eq!(record.loaded_modules[0].module, module1);

    Ok(())
  }
}

mod function_object_properties_smoke {
  use vm_js::{
    Heap, HeapLimits, NativeFunctionId, PropertyDescriptor, PropertyKey, PropertyKind, Value,
    VmError,
  };

  #[test]
  fn function_objects_support_properties_and_prototype_chain_smoke() -> Result<(), VmError> {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut scope = heap.scope();

    let name = scope.alloc_string("f")?;
    let func = scope.alloc_native_function(NativeFunctionId(1), None, name, 0)?;
    scope.push_root(Value::Object(func))?;

    // Own data property on a function object.
    let x_key_s = scope.alloc_string("x")?;
    scope.push_root(Value::String(x_key_s))?;
    let x_key = PropertyKey::from_string(x_key_s);
    scope.define_property(
      func,
      x_key,
      PropertyDescriptor {
        enumerable: true,
        configurable: true,
        kind: PropertyKind::Data {
          value: Value::Number(1.0),
          writable: true,
        },
      },
    )?;
    assert_eq!(
      scope
        .heap()
        .object_get_own_data_property_value(func, &x_key)?,
      Some(Value::Number(1.0))
    );

    // Mutate that property via a Heap API that historically rejected function objects.
    scope
      .heap_mut()
      .object_set_existing_data_property_value(func, &x_key, Value::Number(2.0))?;
    assert_eq!(
      scope
        .heap()
        .object_get_own_data_property_value(func, &x_key)?,
      Some(Value::Number(2.0))
    );

    // Prototype chain lookup where the receiver is a function object.
    let proto = scope.alloc_object()?;
    scope.push_root(Value::Object(proto))?;
    scope.heap_mut().object_set_prototype(func, Some(proto))?;

    let y_key_s = scope.alloc_string("y")?;
    scope.push_root(Value::String(y_key_s))?;
    let y_key = PropertyKey::from_string(y_key_s);
    scope.define_property(
      proto,
      y_key,
      PropertyDescriptor {
        enumerable: true,
        configurable: true,
        kind: PropertyKind::Data {
          value: Value::Number(42.0),
          writable: true,
        },
      },
    )?;

    let desc = scope
      .heap()
      .get_property(func, &y_key)?
      .expect("prototype property should be found via get_property");
    let PropertyKind::Data { value, .. } = desc.kind else {
      panic!("expected a data property");
    };
    assert_eq!(value, Value::Number(42.0));

    Ok(())
  }
}

mod new_target_smoke {
  use super::*;

  #[test]
  fn new_target_is_undefined_in_plain_call() {
    let mut rt = new_runtime();
    let value = rt
      .exec_script(r#"function C(){ return new.target; } C()"#)
      .unwrap();
    assert_eq!(value, Value::Undefined);
  }

  #[test]
  fn new_target_is_constructor_in_construct_call() {
    let mut rt = new_runtime();
    let value = rt
      .exec_script(r#"function C(){ return new.target; } var x = new C(); x === C"#)
      .unwrap();
    assert_eq!(value, Value::Bool(true));
  }

  #[test]
  fn new_target_is_not_propagated_into_nested_plain_calls() {
    let mut rt = new_runtime();
    let value = rt
      .exec_script(
        r#"
        function C(){
          function inner(){ return new.target; }
          this.ok = (inner() === undefined);
        }
        (new C()).ok === true
      "#,
      )
      .unwrap();
    assert_eq!(value, Value::Bool(true));
  }
}

mod optional_chaining_this {
  use super::*;

  #[test]
  fn optional_chain_call_preserves_this_binding() {
    let mut rt = new_runtime();
    let value = rt
      .exec_script(r#"function f(){ return this === o; } var o = {}; o.f = f; o?.f() === true"#)
      .unwrap();
    assert_eq!(value, Value::Bool(true));
  }

  #[test]
  fn optional_call_on_property_preserves_this_binding() {
    let mut rt = new_runtime();
    let value = rt
      .exec_script(r#"function f(){ return this === o; } var o = {}; o.f = f; o.f?.() === true"#)
      .unwrap();
    assert_eq!(value, Value::Bool(true));
  }

  #[test]
  fn optional_call_does_not_evaluate_arguments_when_short_circuited() {
    let mut rt = new_runtime();
    let value = rt
      .exec_script(
        r#"
        var called = false;
        function arg(){ called = true; return 1; }
        var o = null;
        (o?.f(arg()) === undefined) && (called === false)
      "#,
      )
      .unwrap();
    assert_eq!(value, Value::Bool(true));
  }

  #[test]
  fn optional_call_on_identifier_uses_function_call_this_binding_rules() {
    // `f?.()` is an optional call on an IdentifierReference, so it should behave like `f()`.
    let mut rt = new_runtime();
    let value = rt
      .exec_script(r#"function f(){ return this === globalThis; } f?.()"#)
      .unwrap();
    assert_eq!(value, Value::Bool(true));

    let mut rt = new_runtime();
    let value = rt
      .exec_script(r#""use strict"; function f(){ return this === undefined; } f?.()"#)
      .unwrap();
    assert_eq!(value, Value::Bool(true));
  }
}

mod promise_smoke {
  use vm_js::{
    Heap, HeapLimits, JobCallback, PromiseReaction, PromiseReactionType, Value, VmError,
  };

  #[test]
  fn promise_result_is_traced_by_gc_and_brand_check_works() -> Result<(), VmError> {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

    let referenced;
    {
      let mut scope = heap.scope();

      let promise = scope.alloc_promise()?;
      referenced = scope.alloc_object()?;

      assert!(scope.heap().is_promise_object(promise));
      assert!(!scope.heap().is_promise_object(referenced));

      scope
        .heap_mut()
        .promise_fulfill(promise, Value::Object(referenced))?;

      scope.push_root(Value::Object(promise))?;
      scope.heap_mut().collect_garbage();

      assert!(
        scope.heap().is_valid_object(referenced),
        "promise.[[PromiseResult]] should be traced"
      );
    }

    // Stack roots were removed when the scope was dropped, so the result object should now be
    // collectable.
    heap.collect_garbage();
    assert!(!heap.is_valid_object(referenced));
    Ok(())
  }

  #[test]
  fn promise_reaction_lists_are_cleared_on_settlement() -> Result<(), VmError> {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));

    let handler;
    {
      let mut scope = heap.scope();

      let promise = scope.alloc_promise()?;
      handler = scope.alloc_object()?;

      scope.promise_append_fulfill_reaction(
        promise,
        PromiseReaction {
          capability: None,
          type_: PromiseReactionType::Fulfill,
          handler: Some(JobCallback::new(handler)?),
        },
      )?;

      scope.push_root(Value::Object(promise))?;

      // While the promise is pending, its reaction lists keep handlers alive.
      scope.heap_mut().collect_garbage();
      assert!(scope.heap().is_valid_object(handler));

      // Settlement clears the reaction lists so they do not keep handlers alive unnecessarily.
      scope
        .heap_mut()
        .promise_fulfill(promise, Value::Undefined)?;
      scope.heap_mut().collect_garbage();
      assert!(!scope.heap().is_valid_object(handler));
    }

    Ok(())
  }
}

mod promise_job_rooting {
  use vm_js::{
    create_promise_resolve_thenable_job, new_promise_reaction_job, GcObject, Heap, HeapLimits, Job,
    JobCallback, PromiseReactionRecord, PromiseReactionType, RootId, Scope, Value, Vm, VmError,
    VmHost, VmHostHooks, VmJobContext, VmOptions, WeakGcObject,
  };

  fn noop(
    _vm: &mut Vm,
    _scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    _hooks: &mut dyn VmHostHooks,
    _callee: GcObject,
    _this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    Ok(Value::Undefined)
  }

  struct RootingContext<'a> {
    heap: &'a mut Heap,
  }

  impl VmJobContext for RootingContext<'_> {
    fn call(
      &mut self,
      _host: &mut dyn VmHostHooks,
      _callee: Value,
      _this: Value,
      _args: &[Value],
    ) -> Result<Value, VmError> {
      Err(VmError::Unimplemented("RootingContext::call"))
    }

    fn construct(
      &mut self,
      _host: &mut dyn VmHostHooks,
      _callee: Value,
      _args: &[Value],
      _new_target: Value,
    ) -> Result<Value, VmError> {
      Err(VmError::Unimplemented("RootingContext::construct"))
    }

    fn add_root(&mut self, value: Value) -> Result<RootId, VmError> {
      self.heap.add_root(value)
    }

    fn remove_root(&mut self, id: RootId) {
      self.heap.remove_root(id)
    }
  }

  #[derive(Clone)]
  struct TestHost {
    call_result: Result<Value, VmError>,
  }

  impl VmHostHooks for TestHost {
    fn host_enqueue_promise_job(&mut self, _job: Job, _realm: Option<vm_js::RealmId>) {
      // Not used by these tests; we run jobs directly.
    }

    fn host_make_job_callback(&mut self, callback: GcObject) -> Result<JobCallback, VmError> {
      JobCallback::try_new(callback)
    }

    fn host_call_job_callback(
      &mut self,
      _ctx: &mut dyn VmJobContext,
      _callback: &JobCallback,
      _this_argument: Value,
      _arguments: &[Value],
    ) -> Result<Value, VmError> {
      self.call_result.clone()
    }
  }

  #[test]
  fn promise_thenable_job_discard_releases_roots() -> Result<(), VmError> {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut vm = Vm::new(VmOptions::default());

    let call_id = vm.register_native_call(noop)?;

    let then_action;
    let thenable;
    let resolve;
    let reject;
    let job;
    {
      let mut scope = heap.scope();

      let name = scope.alloc_string("then")?;
      then_action = scope.alloc_native_function(call_id, None, name, 2)?;
      thenable = scope.alloc_object()?;
      resolve = scope.alloc_object()?;
      reject = scope.alloc_object()?;

      let mut host = TestHost {
        call_result: Ok(Value::Undefined),
      };
      let created = create_promise_resolve_thenable_job(
        &mut host,
        scope.heap_mut(),
        Value::Object(thenable),
        Value::Object(then_action),
        Value::Object(resolve),
        Value::Object(reject),
      )?
      .expect("then_action is callable");
      job = created;
    }

    let weak_then_action = WeakGcObject::from(then_action);
    let weak_thenable = WeakGcObject::from(thenable);
    let weak_resolve = WeakGcObject::from(resolve);
    let weak_reject = WeakGcObject::from(reject);

    // The job should keep all captured values alive until it runs or is discarded.
    heap.collect_garbage();
    assert!(weak_then_action.upgrade(&heap).is_some());
    assert!(weak_thenable.upgrade(&heap).is_some());
    assert!(weak_resolve.upgrade(&heap).is_some());
    assert!(weak_reject.upgrade(&heap).is_some());

    let mut ctx = RootingContext { heap: &mut heap };
    job.discard(&mut ctx);

    ctx.heap.collect_garbage();
    assert!(weak_then_action.upgrade(&*ctx.heap).is_none());
    assert!(weak_thenable.upgrade(&*ctx.heap).is_none());
    assert!(weak_resolve.upgrade(&*ctx.heap).is_none());
    assert!(weak_reject.upgrade(&*ctx.heap).is_none());

    Ok(())
  }

  #[test]
  fn promise_thenable_job_error_still_releases_roots() -> Result<(), VmError> {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut vm = Vm::new(VmOptions::default());

    let call_id = vm.register_native_call(noop)?;

    let then_action;
    let thenable;
    let resolve;
    let reject;
    let job;
    let mut host = TestHost {
      call_result: Err(VmError::Unimplemented("host_call_job_callback failed")),
    };

    {
      let mut scope = heap.scope();

      let name = scope.alloc_string("then")?;
      then_action = scope.alloc_native_function(call_id, None, name, 2)?;
      thenable = scope.alloc_object()?;
      resolve = scope.alloc_object()?;
      reject = scope.alloc_object()?;

      let created = create_promise_resolve_thenable_job(
        &mut host,
        scope.heap_mut(),
        Value::Object(thenable),
        Value::Object(then_action),
        Value::Object(resolve),
        Value::Object(reject),
      )?
      .expect("then_action is callable");
      job = created;
    }

    let weak_then_action = WeakGcObject::from(then_action);
    let weak_thenable = WeakGcObject::from(thenable);
    let weak_resolve = WeakGcObject::from(resolve);
    let weak_reject = WeakGcObject::from(reject);

    heap.collect_garbage();
    assert!(weak_then_action.upgrade(&heap).is_some());
    assert!(weak_thenable.upgrade(&heap).is_some());
    assert!(weak_resolve.upgrade(&heap).is_some());
    assert!(weak_reject.upgrade(&heap).is_some());

    let mut ctx = RootingContext { heap: &mut heap };

    let err = job
      .run(&mut ctx, &mut host)
      .expect_err("host should return error");
    assert!(matches!(
      err,
      VmError::Unimplemented("host_call_job_callback failed")
    ));

    ctx.heap.collect_garbage();
    assert!(weak_then_action.upgrade(&*ctx.heap).is_none());
    assert!(weak_thenable.upgrade(&*ctx.heap).is_none());
    assert!(weak_resolve.upgrade(&*ctx.heap).is_none());
    assert!(weak_reject.upgrade(&*ctx.heap).is_none());

    Ok(())
  }

  #[test]
  fn promise_reaction_job_discard_releases_roots() -> Result<(), VmError> {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut vm = Vm::new(VmOptions::default());

    let call_id = vm.register_native_call(noop)?;

    let callback;
    let argument;
    let job;
    {
      let mut scope = heap.scope();

      let name = scope.alloc_string("onFulfilled")?;
      callback = scope.alloc_native_function(call_id, None, name, 1)?;
      argument = scope.alloc_object()?;

      let mut host = TestHost {
        call_result: Ok(Value::Undefined),
      };

      let reaction = PromiseReactionRecord {
        reaction_type: PromiseReactionType::Fulfill,
        handler: Some(host.host_make_job_callback(callback)?),
      };
      job = new_promise_reaction_job(scope.heap_mut(), reaction, Value::Object(argument))?;
    }

    let weak_callback = WeakGcObject::from(callback);
    let weak_argument = WeakGcObject::from(argument);

    heap.collect_garbage();
    assert!(weak_callback.upgrade(&heap).is_some());
    assert!(weak_argument.upgrade(&heap).is_some());

    let mut ctx = RootingContext { heap: &mut heap };
    job.discard(&mut ctx);

    ctx.heap.collect_garbage();
    assert!(weak_callback.upgrade(&*ctx.heap).is_none());
    assert!(weak_argument.upgrade(&*ctx.heap).is_none());

    Ok(())
  }

  #[test]
  fn promise_reaction_job_error_still_releases_roots() -> Result<(), VmError> {
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let mut vm = Vm::new(VmOptions::default());

    let call_id = vm.register_native_call(noop)?;

    let callback;
    let argument;
    let job;
    let mut host = TestHost {
      call_result: Err(VmError::Unimplemented("host_call_job_callback failed")),
    };
    {
      let mut scope = heap.scope();

      let name = scope.alloc_string("onFulfilled")?;
      callback = scope.alloc_native_function(call_id, None, name, 1)?;
      argument = scope.alloc_object()?;

      let reaction = PromiseReactionRecord {
        reaction_type: PromiseReactionType::Fulfill,
        handler: Some(host.host_make_job_callback(callback)?),
      };
      job = new_promise_reaction_job(scope.heap_mut(), reaction, Value::Object(argument))?;
    }

    let weak_callback = WeakGcObject::from(callback);
    let weak_argument = WeakGcObject::from(argument);

    heap.collect_garbage();
    assert!(weak_callback.upgrade(&heap).is_some());
    assert!(weak_argument.upgrade(&heap).is_some());

    let mut ctx = RootingContext { heap: &mut heap };
    let err = job
      .run(&mut ctx, &mut host)
      .expect_err("host should return error");
    assert!(matches!(
      err,
      VmError::Unimplemented("host_call_job_callback failed")
    ));

    ctx.heap.collect_garbage();
    assert!(weak_callback.upgrade(&*ctx.heap).is_none());
    assert!(weak_argument.upgrade(&*ctx.heap).is_none());

    Ok(())
  }
}

mod object_builtins_smoke {
  use vm_js::{
    GcObject, Heap, HeapLimits, PropertyDescriptor, PropertyKey, PropertyKind, Realm, Scope, Value,
    Vm, VmError, VmHost, VmHostHooks, VmOptions,
  };

  struct TestRealm {
    vm: Vm,
    heap: Heap,
    realm: Realm,
  }

  fn return_two_native(
    _vm: &mut Vm,
    _scope: &mut Scope<'_>,
    _host: &mut dyn VmHost,
    _hooks: &mut dyn VmHostHooks,
    _callee: GcObject,
    _this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    Ok(Value::Number(2.0))
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

  fn define_enumerable_data_property(
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

  fn to_utf8_string(heap: &Heap, value: Value) -> String {
    let Value::String(s) = value else {
      panic!("expected string value");
    };
    heap.get_string(s).unwrap().to_utf8_lossy()
  }

  #[test]
  fn object_builtins_smoke() -> Result<(), VmError> {
    let mut rt = TestRealm::new()?;

    let object = rt.realm.intrinsics().object_constructor();

    let mut scope = rt.heap.scope();
    #[derive(Default)]
    struct NoopHostHooks;

    impl vm_js::VmHostHooks for NoopHostHooks {
      fn host_enqueue_promise_job(&mut self, _job: vm_js::Job, _realm: Option<vm_js::RealmId>) {
        panic!("unexpected Promise job enqueued during Object builtins smoke test");
      }
    }

    let mut host_hooks = NoopHostHooks::default();

    // Global binding exists and is callable.
    assert_eq!(
      get_own_data_property(&mut scope, rt.realm.global_object(), "Object")?,
      Some(Value::Object(object))
    );
    let _ = rt.vm.call_with_host(
      &mut scope,
      &mut host_hooks,
      Value::Object(object),
      Value::Undefined,
      &[],
    )?;

    // Object.defineProperty
    let define_property = get_own_data_property(&mut scope, object, "defineProperty")?
      .expect("Object.defineProperty should exist");
    let Value::Object(define_property) = define_property else {
      panic!("Object.defineProperty should be a function object");
    };

    let o = scope.alloc_object()?;
    scope.push_root(Value::Object(o))?;

    // { value: 1 }
    let desc = scope.alloc_object()?;
    scope.push_root(Value::Object(desc))?;
    define_enumerable_data_property(&mut scope, desc, "value", Value::Number(1.0))?;

    let x = scope.alloc_string("x")?;
    let args = [Value::Object(o), Value::String(x), Value::Object(desc)];
    let _ = rt.vm.call_with_host(
      &mut scope,
      &mut host_hooks,
      Value::Object(define_property),
      Value::Object(object),
      &args,
    )?;

    let x_key = PropertyKey::from_string(x);
    assert_eq!(
      scope.heap().object_get_own_data_property_value(o, &x_key)?,
      Some(Value::Number(1.0))
    );

    // Object.create + Object.getPrototypeOf
    let create =
      get_own_data_property(&mut scope, object, "create")?.expect("Object.create should exist");
    let Value::Object(create) = create else {
      panic!("Object.create should be a function object");
    };

    let get_proto = get_own_data_property(&mut scope, object, "getPrototypeOf")?
      .expect("Object.getPrototypeOf should exist");
    let Value::Object(get_proto) = get_proto else {
      panic!("Object.getPrototypeOf should be a function object");
    };

    // { y: 2 }
    let p = scope.alloc_object()?;
    scope.push_root(Value::Object(p))?;
    define_enumerable_data_property(&mut scope, p, "y", Value::Number(2.0))?;
    let y_key = PropertyKey::from_string(scope.alloc_string("y")?);

    let args = [Value::Object(p)];
    let created = rt.vm.call_with_host(
      &mut scope,
      &mut host_hooks,
      Value::Object(create),
      Value::Object(object),
      &args,
    )?;
    let Value::Object(created) = created else {
      panic!("Object.create should return an object");
    };
    scope.push_root(Value::Object(created))?;

    // Inherited property lookup via prototype chain.
    let desc = scope
      .heap()
      .get_property(created, &y_key)?
      .expect("property should be found via prototype");
    let PropertyKind::Data { value, .. } = desc.kind else {
      panic!("expected data property");
    };
    assert_eq!(value, Value::Number(2.0));

    // getPrototypeOf(created) === p
    let args = [Value::Object(created)];
    let proto = rt.vm.call_with_host(
      &mut scope,
      &mut host_hooks,
      Value::Object(get_proto),
      Value::Object(object),
      &args,
    )?;
    assert_eq!(proto, Value::Object(p));

    // Object.keys
    let keys =
      get_own_data_property(&mut scope, object, "keys")?.expect("Object.keys should exist");
    let Value::Object(keys) = keys else {
      panic!("Object.keys should be a function object");
    };

    let obj = scope.alloc_object()?;
    scope.push_root(Value::Object(obj))?;
    define_enumerable_data_property(&mut scope, obj, "a", Value::Number(1.0))?;
    define_enumerable_data_property(&mut scope, obj, "b", Value::Number(2.0))?;

    let args = [Value::Object(obj)];
    let result = rt.vm.call_with_host(
      &mut scope,
      &mut host_hooks,
      Value::Object(keys),
      Value::Object(object),
      &args,
    )?;
    let Value::Object(arr) = result else {
      panic!("Object.keys should return an object");
    };

    let length = get_own_data_property(&mut scope, arr, "length")?.expect("length should exist");
    assert_eq!(length, Value::Number(2.0));

    // Keys are returned in insertion order for non-index string keys.
    let first = get_own_data_property(&mut scope, arr, "0")?.expect("key 0 should exist");
    let second = get_own_data_property(&mut scope, arr, "1")?.expect("key 1 should exist");
    assert_eq!(to_utf8_string(scope.heap(), first), "a");
    assert_eq!(to_utf8_string(scope.heap(), second), "b");

    // Object.assign
    let assign =
      get_own_data_property(&mut scope, object, "assign")?.expect("Object.assign should exist");
    let Value::Object(assign) = assign else {
      panic!("Object.assign should be a function object");
    };

    let target = scope.alloc_object()?;
    scope.push_root(Value::Object(target))?;
    let source = scope.alloc_object()?;
    scope.push_root(Value::Object(source))?;
    define_enumerable_data_property(&mut scope, source, "a", Value::Number(1.0))?;

    // Enumerable accessor property: ensure `Object.assign` invokes getters (`Get` semantics).
    let getter_id = rt.vm.register_native_call(return_two_native)?;
    let getter_name = scope.alloc_string("")?;
    let getter = scope.alloc_native_function(getter_id, None, getter_name, 0)?;
    scope.push_root(Value::Object(getter))?;
    let key_b = PropertyKey::from_string(scope.alloc_string("b")?);
    scope.define_property(
      source,
      key_b,
      PropertyDescriptor {
        enumerable: true,
        configurable: true,
        kind: PropertyKind::Accessor {
          get: Value::Object(getter),
          set: Value::Undefined,
        },
      },
    )?;

    let args = [Value::Object(target), Value::Object(source)];
    let out = rt.vm.call_with_host(
      &mut scope,
      &mut host_hooks,
      Value::Object(assign),
      Value::Object(object),
      &args,
    )?;
    assert_eq!(out, Value::Object(target));
    assert_eq!(
      get_own_data_property(&mut scope, target, "a")?,
      Some(Value::Number(1.0))
    );
    assert_eq!(
      get_own_data_property(&mut scope, target, "b")?,
      Some(Value::Number(2.0))
    );

    // Assigning onto a non-writable target property should throw (failed `Set`).
    let ro_target = scope.alloc_object()?;
    scope.push_root(Value::Object(ro_target))?;
    let ro_source = scope.alloc_object()?;
    scope.push_root(Value::Object(ro_source))?;

    let key_x = PropertyKey::from_string(scope.alloc_string("x")?);
    scope.define_property(
      ro_target,
      key_x,
      PropertyDescriptor {
        enumerable: true,
        configurable: true,
        kind: PropertyKind::Data {
          value: Value::Number(1.0),
          writable: false,
        },
      },
    )?;
    define_enumerable_data_property(&mut scope, ro_source, "x", Value::Number(2.0))?;

    let args = [Value::Object(ro_target), Value::Object(ro_source)];
    let err = rt
      .vm
      .call_with_host(
        &mut scope,
        &mut host_hooks,
        Value::Object(assign),
        Value::Object(object),
        &args,
      )
      .unwrap_err();
    match &err {
      VmError::TypeError(_) => {}
      _ => {
        let Some(thrown) = err.thrown_value() else {
          panic!("expected TypeError, got {err:?}");
        };
        let Value::Object(thrown_obj) = thrown else {
          panic!("expected thrown TypeError object, got {thrown:?}");
        };
        let name = get_own_data_property(&mut scope, thrown_obj, "name")?
          .expect("thrown error object should have a 'name' property");
        assert_eq!(to_utf8_string(scope.heap(), name), "TypeError");
      }
    }

    // Object.setPrototypeOf
    let set_proto = get_own_data_property(&mut scope, object, "setPrototypeOf")?
      .expect("Object.setPrototypeOf should exist");
    let Value::Object(set_proto) = set_proto else {
      panic!("Object.setPrototypeOf should be a function object");
    };

    let obj = scope.alloc_object()?;
    scope.push_root(Value::Object(obj))?;
    let args = [Value::Object(obj), Value::Object(p)];
    let out = rt.vm.call_with_host(
      &mut scope,
      &mut host_hooks,
      Value::Object(set_proto),
      Value::Object(object),
      &args,
    )?;
    assert_eq!(out, Value::Object(obj));

    let args = [Value::Object(obj)];
    let proto = rt.vm.call_with_host(
      &mut scope,
      &mut host_hooks,
      Value::Object(get_proto),
      Value::Object(object),
      &args,
    )?;
    assert_eq!(proto, Value::Object(p));

    Ok(())
  }
}

mod webidl_vmjs_promise_resolve_smoke {
  use crate::js::webidl::{
    InterfaceId, VmJsWebIdlBindingsCx, VmJsWebIdlBindingsState, WebIdlHooks, WebIdlLimits,
  };
  use vm_js::{Heap, HeapLimits, PromiseState, Realm, Value, Vm, VmError, VmOptions};
  use webidl_js_runtime::WebIdlJsRuntime as _;

  #[derive(Default)]
  struct NoHooks;

  impl WebIdlHooks<Value> for NoHooks {
    fn is_platform_object(&self, _value: Value) -> bool {
      false
    }

    fn implements_interface(&self, _value: Value, _interface: InterfaceId) -> bool {
      false
    }
  }

  #[test]
  fn vmjs_webidl_runtime_adapter_promise_resolve_smoke() -> Result<(), VmError> {
    let mut vm = Vm::new(VmOptions::default());
    let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
    let realm = Realm::new(&mut vm, &mut heap)?;

    let state = VmJsWebIdlBindingsState::<()>::new(
      realm.global_object(),
      WebIdlLimits::default(),
      Box::new(NoHooks),
    );

    // Create a resolved promise from a non-object value.
    let promise;
    let promise_again;
    {
      let mut cx = VmJsWebIdlBindingsCx::new(&mut vm, &mut heap, &state);
      promise = cx.promise_resolve(Value::Number(7.0))?;
      // PromiseResolve(%Promise%, promise) should return the promise unchanged when it uses the
      // intrinsic Promise constructor.
      promise_again = cx.promise_resolve(promise)?;
      assert_eq!(promise, promise_again);
    }

    let Value::Object(promise_obj) = promise else {
      panic!("expected promise_resolve to return a Promise object, got {promise:?}");
    };
    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Fulfilled);
    assert_eq!(heap.promise_result(promise_obj)?, Some(Value::Number(7.0)));

    realm.teardown(&mut heap);
    Ok(())
  }
}
