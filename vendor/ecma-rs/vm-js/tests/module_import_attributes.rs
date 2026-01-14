use vm_js::{
  import_attributes_from_options, Heap, HeapLimits, HostDefined, ImportAttribute, Job, JsRuntime, JsString,
  MicrotaskQueue, ModuleGraph, ModuleLoadPayload, ModuleReferrer, ModuleRequest, SourceTextModuleRecord, Value,
  Vm, VmError, VmHostHooks, VmOptions,
};

fn assert_syntax_error(source: &str) {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  match SourceTextModuleRecord::parse(&mut heap, source) {
    Err(VmError::Syntax(_)) => {}
    other => panic!("expected VmError::Syntax, got {other:?}"),
  }
}

#[test]
fn parses_import_with_attributes() {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let module =
    SourceTextModuleRecord::parse(&mut heap, r#"import x from "m" with { type: "json" };"#).unwrap();
  assert_eq!(
    module.requested_modules,
    vec![ModuleRequest::new(
      JsString::from_str("m").unwrap(),
      vec![ImportAttribute::try_new("type", "json").unwrap()],
    )]
  );
}

#[test]
fn parses_export_with_attributes() {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let module =
    SourceTextModuleRecord::parse(&mut heap, r#"export * from "m" with { type: "json" };"#).unwrap();
  assert_eq!(
    module.requested_modules,
    vec![ModuleRequest::new(
      JsString::from_str("m").unwrap(),
      vec![ImportAttribute::try_new("type", "json").unwrap()],
    )]
  );
}

#[test]
fn sorts_attributes_and_dedupes_requested_modules() {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let module = SourceTextModuleRecord::parse(
    &mut heap,
    r#"
      import x from "m" with { b: "2", a: "1" };
      import y from "m" with { a: "1", b: "2" };
    "#,
  )
  .unwrap();

  assert_eq!(module.requested_modules.len(), 1);
  assert_eq!(
    module.requested_modules[0].attributes,
    vec![
      ImportAttribute::try_new("a", "1").unwrap(),
      ImportAttribute::try_new("b", "2").unwrap(),
    ]
  );
}

#[test]
fn requested_modules_distinguish_different_attributes() {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let module = SourceTextModuleRecord::parse(
    &mut heap,
    r#"
      import x from "m" with { type: "json" };
      import y from "m" with { type: "css" };
    "#,
  )
  .unwrap();
  assert_eq!(module.requested_modules.len(), 2);
}

#[test]
fn rejects_invalid_attribute_shapes() {
  assert_syntax_error(r#"import x from "m" with 1;"#);
  assert_syntax_error(r#"import x from "m" with { ["type"]: "json" };"#);
  assert_syntax_error(r#"import x from "m" with { type: 1 };"#);
  assert_syntax_error(r#"import x from "m" with { type: "json", type: "json" };"#);
}

#[test]
fn static_import_attribute_value_preserves_unpaired_surrogate() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let record = SourceTextModuleRecord::parse(
    &mut heap,
    r#"import x from "m" with { type: "\uD800" };"#,
  )?;
  assert_eq!(record.requested_modules.len(), 1);
  assert_eq!(record.requested_modules[0].attributes.len(), 1);
  assert_eq!(
    record.requested_modules[0].attributes[0].value.as_code_units(),
    &[0xD800u16]
  );
  Ok(())
}

#[test]
fn static_import_attribute_string_key_preserves_unpaired_surrogate() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let record = SourceTextModuleRecord::parse(
    &mut heap,
    r#"import x from "m" with { "\uD800": "json" };"#,
  )?;
  assert_eq!(record.requested_modules.len(), 1);
  assert_eq!(record.requested_modules[0].attributes.len(), 1);
  assert_eq!(
    record.requested_modules[0].attributes[0].key.as_code_units(),
    &[0xD800u16]
  );
  Ok(())
}

#[test]
fn dynamic_import_attribute_value_preserves_unpaired_surrogate() -> Result<(), VmError> {
  // Dynamic import attribute validation uses a host-provided supported key list. Since Rust `&str`
  // cannot represent unpaired surrogate code points, this test uses a supported ASCII key (`type`)
  // while asserting that the **value** preserves an unpaired surrogate code unit.
  struct Host {
    microtasks: MicrotaskQueue,
    captured_value_units: Option<Vec<u16>>,
  }

  impl VmHostHooks for Host {
    fn host_enqueue_promise_job(&mut self, job: Job, realm: Option<vm_js::RealmId>) {
      self.microtasks.host_enqueue_promise_job(job, realm);
    }

    fn host_get_supported_import_attributes(&self) -> &'static [&'static str] {
      &["type"]
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
      assert_eq!(module_request.attributes.len(), 1);
      let attr = &module_request.attributes[0];
      assert_eq!(attr.key.to_utf8_lossy(), "type");
      self.captured_value_units = Some(attr.value.as_code_units().to_vec());

      // Complete loading immediately so we do not leak roots held by `payload`.
      vm.finish_loading_imported_module(
        scope,
        modules,
        self,
        referrer,
        module_request,
        payload,
        // Reject the dynamic import promise with a thrown value so `ContinueDynamicImport` treats the
        // failure as a promise rejection (not a fatal VM error).
        Err(VmError::Throw(Value::Undefined)),
      )
    }
  }

  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let mut hooks = Host {
    microtasks: MicrotaskQueue::new(),
    captured_value_units: None,
  };

  let exec_res = rt.exec_script_with_hooks(
    &mut hooks,
    r#"import("m", { with: { type: "\uD800" } }).catch(() => {});"#,
  );

  // Discard any queued Promise jobs so `Job` instances don't get dropped with leaked roots even if
  // the script execution failed.
  hooks.microtasks.teardown(&mut rt);

  exec_res?;

  assert_eq!(
    hooks.captured_value_units.as_deref(),
    Some(&[0xD800u16][..])
  );

  Ok(())
}

#[test]
fn import_attributes_from_options_preserves_unpaired_surrogate_key() {
  // Use the lower-level attribute extraction helper so we can inject a key containing an unpaired
  // surrogate code unit (which cannot be represented in Rust `&str`).
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut scope = heap.scope();
  let mut vm = Vm::new(VmOptions::default());

  let options = scope.alloc_object().unwrap();
  let attributes = scope.alloc_object().unwrap();

  // options.with = { "\uD800": "\uD800" }
  let k_with = scope.alloc_string("with").unwrap();
  let key_surrogate = scope.alloc_string_from_code_units(&[0xD800u16]).unwrap();
  let value_surrogate = scope.alloc_string_from_code_units(&[0xD800u16]).unwrap();

  scope
    .define_property(
      attributes,
      vm_js::PropertyKey::String(key_surrogate),
      vm_js::PropertyDescriptor {
        enumerable: true,
        configurable: true,
        kind: vm_js::PropertyKind::Data {
          value: Value::String(value_surrogate),
          writable: true,
        },
      },
    )
    .unwrap();
  scope
    .define_property(
      options,
      vm_js::PropertyKey::String(k_with),
      vm_js::PropertyDescriptor {
        enumerable: true,
        configurable: true,
        kind: vm_js::PropertyKind::Data {
          value: Value::Object(attributes),
          writable: true,
        },
      },
    )
    .unwrap();

  // No supported keys: the helper should reject with UnsupportedImportAttribute, but must preserve
  // the raw UTF-16 code units for the key in the error.
  let err = import_attributes_from_options(&mut vm, &mut scope, Value::Object(options), &[]).unwrap_err();
  match err {
    vm_js::ImportCallError::TypeError(vm_js::ImportCallTypeError::UnsupportedImportAttribute { key }) => {
      assert_eq!(key.as_code_units(), &[0xD800u16]);
    }
    other => panic!("expected unsupported attribute TypeError, got {other:?}"),
  }
}

#[test]
fn module_requests_equal_distinguishes_surrogate_and_replacement_in_attribute_value() {
  // `ModuleRequestsEqual` compares attribute keys/values by UTF-16 code units. An unpaired surrogate
  // must not compare equal to U+FFFD.
  let specifier = JsString::from_str("m").unwrap();
  let key = JsString::from_str("type").unwrap();
  let surrogate = JsString::from_code_units(&[0xD800u16]).unwrap();
  let replacement = JsString::from_str("\u{FFFD}").unwrap();

  let left = ModuleRequest::new(
    specifier.clone(),
    vec![ImportAttribute::new(key.clone(), surrogate)],
  );
  let right = ModuleRequest::new(
    specifier,
    vec![ImportAttribute::new(key, replacement)],
  );

  assert!(!vm_js::module_requests_equal(&left, &right));
  assert_ne!(left, right);
}
