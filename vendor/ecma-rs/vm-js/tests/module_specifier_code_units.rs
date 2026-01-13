use vm_js::{
  Heap, HeapLimits, HostDefined, Job, JsRuntime, JsString, MicrotaskQueue, ModuleGraph,
  ModuleLoadPayload, ModuleReferrer, ModuleRequest, SourceTextModuleRecord, Vm, VmError, VmHostHooks,
  VmOptions, module_requests_equal,
};

#[test]
fn dynamic_import_specifier_preserves_unpaired_surrogate() -> Result<(), VmError> {
  // Dynamic import specifiers are computed at runtime and must preserve UTF-16 code units (including
  // unpaired surrogates) when forwarded to `HostLoadImportedModule`.
  struct Host {
    microtasks: MicrotaskQueue,
    captured_specifier_units: Option<Vec<u16>>,
  }

  impl VmHostHooks for Host {
    fn host_enqueue_promise_job(&mut self, job: Job, realm: Option<vm_js::RealmId>) {
      self.microtasks.host_enqueue_promise_job(job, realm);
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
      self.captured_specifier_units = Some(module_request.specifier.as_code_units().to_vec());

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
        Err(VmError::Throw(vm_js::Value::Undefined)),
      )
    }
  }

  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let mut hooks = Host {
    microtasks: MicrotaskQueue::new(),
    captured_specifier_units: None,
  };

  // Use an unpaired surrogate code unit (U+D800).
  let exec_res = rt.exec_script_with_hooks(&mut hooks, r#"import("\uD800").catch(() => {});"#);

  // Discard any queued Promise jobs so `Job` instances don't get dropped with leaked roots even if
  // the script execution failed.
  hooks.microtasks.teardown(&mut rt);

  exec_res?;

  assert_eq!(
    hooks.captured_specifier_units.as_deref(),
    Some(&[0xD800u16][..])
  );
  Ok(())
}

#[test]
fn static_import_specifier_preserves_unpaired_surrogate() -> Result<(), VmError> {
  // Static import specifiers come from string literals and must preserve UTF-16 code units.
  let mut heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let record = SourceTextModuleRecord::parse(&mut heap, r#"import x from "\uD800";"#)?;
  assert_eq!(record.requested_modules.len(), 1);
  assert_eq!(
    record.requested_modules[0].specifier.as_code_units(),
    &[0xD800u16]
  );
  Ok(())
}

#[test]
fn module_requests_equal_distinguishes_surrogate_and_replacement_char() {
  // `ModuleRequestsEqual` compares specifiers by UTF-16 code units. An unpaired surrogate must not
  // compare equal to U+FFFD.
  let surrogate = JsString::from_code_units(&[0xD800u16]).unwrap();
  let replacement = JsString::from_str("\u{FFFD}").unwrap();

  let left = ModuleRequest::new(surrogate, vec![]);
  let right = ModuleRequest::new(replacement, vec![]);

  assert!(!module_requests_equal(&left, &right));
  assert_ne!(left, right);
}
