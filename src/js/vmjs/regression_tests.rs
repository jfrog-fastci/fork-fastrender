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

mod module_graph_loader_smoke {
  use std::collections::VecDeque;
  use vm_js::{
    load_requested_modules, HostDefined, Job, ModuleGraph, ModuleId, ModuleLoadPayload,
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
      let loaded = modules.add_module(SourceTextModuleRecord::default());
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
  fn module_graph_loader_caches_loaded_modules_and_resolves_promise() -> Result<(), VmError> {
    let mut rt = TestRealm::new()?;
    let mut scope = rt.heap.scope();
    let mut modules = ModuleGraph::default();
    let mut host = TestHost::new();

    let request = ModuleRequest::new("dep.js", vec![]);
    let mut referrer_record = SourceTextModuleRecord::default();
    referrer_record.requested_modules.push(request.clone());
    let referrer = modules.add_module(referrer_record);

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
      panic!("expected module graph loader to return a Promise object");
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
  fn module_graph_loader_rejects_duplicate_loaded_module_mismatch() -> Result<(), VmError> {
    let mut rt = TestRealm::new()?;
    let mut scope = rt.heap.scope();
    let mut modules = ModuleGraph::default();
    let module1 = modules.add_module(SourceTextModuleRecord::default());
    let module2 = modules.add_module(SourceTextModuleRecord::default());

    let request_dup = ModuleRequest::new("dup.js", vec![]);
    let mut referrer_record = SourceTextModuleRecord::default();
    // Intentionally create duplicate entries in `[[RequestedModules]]` so the loader invokes the host
    // hook twice and exercises `finish_loading_imported_module`'s caching/mismatch logic.
    referrer_record.requested_modules = vec![request_dup.clone(), request_dup.clone()];
    let referrer_module = modules.add_module(referrer_record);

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
      panic!("expected module graph loader to return a Promise object");
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
