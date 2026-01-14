use crate::executor::{ExecError, ExecPhase, ExecResult, Executor, JsError};
use crate::harness::MODULE_SEPARATOR_MARKER;
use crate::report::Variant;
use crate::runner::TestCase;
use diagnostics::render::render_diagnostic;
use diagnostics::SimpleFiles;
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use vm_js::format_stack_trace;
use vm_js::{
  finish_loading_imported_module, HostDefined, ImportAttribute, ImportMetaProperty, Job,
  ModuleGraph, ModuleId, ModuleLoadPayload, ModuleReferrer, ModuleRequest, PromiseState, RealmId,
  RootId, VmHost, VmHostHooks, VmJobContext,
};
use vm_js::{
  GcObject, Heap, HeapLimits, Intrinsics, MicrotaskQueue, PropertyDescriptor, PropertyKey,
  PropertyKind, Realm, SourceText, SourceTextModuleRecord, StackFrame, TerminationReason, Value,
  Vm, VmError, VmOptions,
};

// Some test262 cases intentionally construct very large strings (e.g. using
// `"0".repeat(2 ** 24)` to stress parser scanning logic). Keep the default heap
// large enough that those tests exercise the VM rather than failing with OOM in
// the harness.
const DEFAULT_HEAP_MAX_BYTES: usize = 512 * 1024 * 1024;
const DEFAULT_HEAP_GC_THRESHOLD_BYTES: usize = 64 * 1024 * 1024;

// Some test262 cases (notably PTC/TCO tests) intentionally recurse extremely deeply.
//
// `vm-js` has its own stack-depth check (`VmOptions::max_stack_depth`), but the interpreter currently
// uses host recursion heavily enough that running with the default OS thread stack can still abort
// the entire `test262-semantic` process with:
//   `fatal runtime error: stack overflow`
//
// To keep the harness robust (and allow collecting a full JSON report even when PTC is not
// supported), run each test case on a fresh OS thread with a larger stack.
const TEST_CASE_THREAD_STACK_SIZE: usize = 128 * 1024 * 1024;

#[derive(Debug, Clone, Default)]
struct AsyncDoneError {
  typ: Option<String>,
  message: String,
}

#[derive(Debug, Default)]
struct AsyncDoneState {
  called: bool,
  error: Option<AsyncDoneError>,
}

impl AsyncDoneState {
  fn is_complete(&self) -> bool {
    self.called
  }
}

fn done_native_call(
  vm: &mut Vm,
  scope: &mut vm_js::Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  // First call wins; ignore repeats for determinism.
  if vm
    .user_data::<AsyncDoneState>()
    .ok_or(VmError::InvariantViolation("$DONE state missing on VM"))?
    .called
  {
    return Ok(Value::Undefined);
  }

  let arg = args.get(0).copied().unwrap_or(Value::Undefined);
  let error = match arg {
    Value::Undefined | Value::Null => None,
    other => {
      let (typ, message) = describe_done_argument(vm, scope, host, hooks, other)?;
      Some(AsyncDoneError { typ, message })
    }
  };

  let Some(state) = vm.user_data_mut::<AsyncDoneState>() else {
    return Err(VmError::InvariantViolation("$DONE state missing on VM"));
  };
  state.called = true;
  state.error = error;
  Ok(Value::Undefined)
}

fn describe_done_argument(
  vm: &mut Vm,
  scope: &mut vm_js::Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  value: Value,
) -> Result<(Option<String>, String), VmError> {
  // Prefer `error.name` + `error.message` when they are plain string data properties.
  //
  // This keeps messages deterministic (avoids user-defined `toString` / accessors), matching what
  // test262 async harnesses typically print for async failures.
  if let Value::Object(obj) = value {
    let mut inner = scope.reborrow();
    inner.push_root(value)?;
    let name = get_object_string_data_property(&mut inner, obj, "name")
      .or_else(|| get_object_constructor_name(&mut inner, obj));
    let message = get_object_string_data_property(&mut inner, obj, "message");

    if let Some(name) = name {
      let rendered = match message {
        Some(msg) => format!("{name}: {msg}"),
        None => name.clone(),
      };
      return Ok((Some(name), rendered));
    }
    if let Some(message) = message {
      return Ok((None, message));
    }
  }

  // Fallback: spec `ToString(error)`.
  let mut inner = scope.reborrow();
  inner.push_root(value)?;
  let s = inner.to_string(vm, host, hooks, value)?;
  let msg = inner
    .heap()
    .get_string(s)
    .map(|s| s.to_utf8_lossy())
    .unwrap_or_else(|_| "<invalid string>".to_string());
  Ok((None, msg))
}

fn install_done_global(runtime: &mut vm_js::JsRuntime) -> Result<(), VmError> {
  runtime.vm.set_user_data(AsyncDoneState::default());

  let call_id = runtime.vm.register_native_call(done_native_call)?;
  let intr = runtime
    .vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
  let global_object = runtime.realm().global_object();

  let mut scope = runtime.heap.scope();
  let name = scope.alloc_string("$DONE")?;

  let func = scope.alloc_native_function(call_id, None, name, 1)?;
  scope.push_root(Value::Object(func))?;
  scope
    .heap_mut()
    .object_set_prototype(func, Some(intr.function_prototype()))?;

  scope.define_property(
    global_object,
    PropertyKey::from_string(name),
    PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Data {
        value: Value::Object(func),
        writable: true,
      },
    },
  )?;

  Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ModuleCacheKey {
  path: PathBuf,
  attributes: Vec<ImportAttribute>,
}

/// Host hooks used by `VmJsExecutor` when running `Variant::Module` tests.
///
/// This combines:
/// - a host-owned microtask queue (Promise jobs), and
/// - a synchronous file-based module loader (static imports + dynamic `import()`).
#[derive(Debug, Default)]
struct Test262ModuleHooks {
  microtasks: MicrotaskQueue,
  /// Realms created via `$262.createRealm()`.
  ///
  /// Each [`Realm`] owns persistent GC roots and must be explicitly torn down with access to the
  /// [`Heap`] before dropping.
  created_realms: Vec<Realm>,
  /// Directory used to resolve dynamic `import()` from classic scripts.
  ///
  /// `vm-js` passes `ModuleReferrer::Script(_)` when a classic script is the active
  /// `ScriptOrModule`, but some embeddings may still fall back to `ModuleReferrer::Realm(_)` when no
  /// script identity is available.
  test_dir: PathBuf,
  /// Sandbox root for module loading. All resolved module paths must stay within this directory.
  ///
  /// Derived as the nearest ancestor directory of the entry test case whose name is `test`.
  test_root_dir_normalized: PathBuf,
  test_root_dir_canonical: PathBuf,
  module_paths: HashMap<ModuleId, PathBuf>,
  module_urls: HashMap<ModuleId, String>,
  module_cache: HashMap<ModuleCacheKey, ModuleId>,
}

impl Test262ModuleHooks {
  fn new(test_path: &Path) -> Self {
    let test_root_dir = derive_test_root_dir(test_path);
    let test_root_dir_canonical =
      std::fs::canonicalize(&test_root_dir).unwrap_or_else(|_| test_root_dir.clone());
    let test_root_dir_normalized = normalize_path(&test_root_dir_canonical);

    let test_dir = test_path.parent().unwrap_or_else(|| Path::new(""));
    // Canonicalize so that sandbox prefix checks compare like-for-like with module paths.
    let test_dir = std::fs::canonicalize(test_dir).unwrap_or_else(|_| test_dir.to_path_buf());

    Self {
      microtasks: MicrotaskQueue::new(),
      created_realms: Vec::new(),
      test_dir,
      test_root_dir_normalized,
      test_root_dir_canonical,
      module_paths: HashMap::new(),
      module_urls: HashMap::new(),
      module_cache: HashMap::new(),
    }
  }

  fn register_module_path(&mut self, id: ModuleId, path: PathBuf) {
    let url = self.module_url_for_path(&path);
    self.module_paths.insert(id, path);
    self.module_urls.insert(id, url);
  }

  fn register_module_cache(
    &mut self,
    path: PathBuf,
    attributes: Vec<ImportAttribute>,
    id: ModuleId,
  ) {
    self
      .module_cache
      .insert(ModuleCacheKey { path, attributes }, id);
  }

  fn module_url_for_path(&self, path: &Path) -> String {
    let relative = path
      .strip_prefix(&self.test_root_dir_canonical)
      .or_else(|_| path.strip_prefix(&self.test_root_dir_normalized))
      .unwrap_or(path);
    let mut rel_str = relative.to_string_lossy().into_owned();
    if rel_str.contains('\\') {
      rel_str = rel_str.replace('\\', "/");
    }
    // `strip_prefix` should yield a relative path, but keep this deterministic in case the path
    // isn't under the derived test root for some reason (e.g. non-existent paths in unit tests).
    let rel_str = rel_str.trim_start_matches('/');
    format!("test262:///{rel_str}")
  }

  fn resolve_base_dir(&self, referrer: ModuleReferrer) -> Result<PathBuf, VmError> {
    match referrer {
      ModuleReferrer::Module(id) => Ok(
        self
          .module_paths
          .get(&id)
          .and_then(|p| p.parent().map(|p| p.to_path_buf()))
          .unwrap_or_else(|| self.test_dir.clone()),
      ),
      ModuleReferrer::Realm(_) => Ok(self.test_dir.clone()),
      // test262 runs each case as a single entry script, so resolving relative specifiers against
      // the test directory is sufficient (even though we don't track ScriptId->path mappings yet).
      ModuleReferrer::Script(_) => Ok(self.test_dir.clone()),
    }
  }

  fn perform_microtask_checkpoint(&mut self, ctx: &mut dyn VmJobContext) -> Vec<VmError> {
    if !self.microtasks.begin_checkpoint() {
      return Vec::new();
    }

    let mut errors = Vec::new();
    loop {
      let job = match self.microtasks.pop_front() {
        Some((_realm, job)) => job,
        None => break,
      };

      if let Err(err) = job.run(ctx, self) {
        let is_termination = matches!(err, VmError::Termination(_));
        errors.push(err);
        if is_termination {
          // Termination is a hard stop: discard any remaining queued jobs so we don't leak roots.
          self.microtasks.teardown(ctx);
          break;
        }
      }
    }

    self.microtasks.end_checkpoint();
    errors
  }
}

impl VmHostHooks for Test262ModuleHooks {
  fn host_enqueue_promise_job(&mut self, job: Job, realm: Option<RealmId>) {
    self.microtasks.enqueue_promise_job(job, realm);
  }

  fn host_get_supported_import_attributes(&self) -> &'static [&'static str] {
    &["type"]
  }

  fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
    Some(self)
  }

  fn host_get_import_meta_properties(
    &mut self,
    _vm: &mut Vm,
    scope: &mut vm_js::Scope<'_>,
    module: ModuleId,
  ) -> Result<Vec<ImportMetaProperty>, VmError> {
    let Some(url) = self.module_urls.get(&module) else {
      return Ok(Vec::new());
    };

    let key_s = scope.alloc_string("url")?;
    scope.push_root(Value::String(key_s))?;
    let key = PropertyKey::from_string(key_s);

    let url_s = scope.alloc_string(url.as_str())?;
    scope.push_root(Value::String(url_s))?;

    Ok(vec![ImportMetaProperty {
      key,
      value: Value::String(url_s),
    }])
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
    let base_dir = self.resolve_base_dir(referrer)?;
    let specifier = module_request.specifier.clone();
    let specifier_utf8 = specifier.to_utf8_lossy();

    // Validate the import specifier before touching the filesystem.
    if specifier.is_empty() {
      let result = Err(module_load_type_error(
        vm,
        scope,
        "import specifier must not be empty",
      )?);
      return finish_loading_imported_module(
        vm,
        scope,
        modules,
        self,
        referrer,
        module_request,
        payload,
        result,
      );
    }
    if specifier.as_code_units().iter().any(|&u| u == 0) {
      let result = Err(module_load_type_error(
        vm,
        scope,
        "import specifier contains NUL",
      )?);
      return finish_loading_imported_module(
        vm,
        scope,
        modules,
        self,
        referrer,
        module_request,
        payload,
        result,
      );
    }
    if is_absolute_filesystem_specifier(&specifier_utf8) {
      let result = Err(module_load_type_error(
        vm,
        scope,
        "import specifier must not be an absolute path",
      )?);
      return finish_loading_imported_module(
        vm,
        scope,
        modules,
        self,
        referrer,
        module_request,
        payload,
        result,
      );
    }

    enum RequestedModuleKind {
      JavaScript,
      Json,
    }

    #[inline]
    fn js_string_eq_str(js: &vm_js::JsString, s: &str) -> bool {
      js.as_code_units().iter().copied().eq(s.encode_utf16())
    }

    let kind = if module_request.attributes.is_empty() {
      RequestedModuleKind::JavaScript
    } else {
      // `vm-js` performs `AllImportAttributesSupported` using `host_get_supported_import_attributes`
      // before invoking this hook, but be defensive when called directly.
      if let Some(attr) = module_request
        .attributes
        .iter()
        .find(|a| !js_string_eq_str(&a.key, "type"))
      {
        let result = Err(module_load_syntax_error_message(
          vm,
          scope,
          &format!("Unsupported import attribute: {}", attr.key.to_utf8_lossy()),
        )?);
        return finish_loading_imported_module(
          vm,
          scope,
          modules,
          self,
          referrer,
          module_request,
          payload,
          result,
        );
      }

      // Support JSON modules via `with { type: 'json' }`.
      if module_request
        .attributes
        .iter()
        .all(|a| js_string_eq_str(&a.key, "type") && js_string_eq_str(&a.value, "json"))
      {
        RequestedModuleKind::Json
      } else {
        let typ = module_request
          .attributes
          .iter()
          .find(|a| js_string_eq_str(&a.key, "type") && !js_string_eq_str(&a.value, "json"))
          .map(|a| a.value.to_utf8_lossy())
          .unwrap_or_else(|| "<unknown>".to_string());
        let result = Err(module_load_syntax_error_message(
          vm,
          scope,
          &format!("Unsupported module type: {typ}"),
        )?);
        return finish_loading_imported_module(
          vm,
          scope,
          modules,
          self,
          referrer,
          module_request,
          payload,
          result,
        );
      }
    };

    let joined = base_dir.join(&specifier_utf8);
    let normalized = normalize_path(&joined);
    if !normalized.starts_with(&self.test_root_dir_normalized) {
      let result = Err(module_load_type_error(
        vm,
        scope,
        &format!("import specifier escapes test262 sandbox: {specifier_utf8}"),
      )?);
      return finish_loading_imported_module(
        vm,
        scope,
        modules,
        self,
        referrer,
        module_request,
        payload,
        result,
      );
    }

    // Canonicalize for symlink-aware sandboxing. Missing files are rejected deterministically.
    let canonical = match std::fs::canonicalize(&normalized) {
      Ok(path) => {
        if !path.starts_with(&self.test_root_dir_canonical) {
          let result = Err(module_load_type_error(
            vm,
            scope,
            &format!("import specifier escapes test262 sandbox: {specifier_utf8}"),
          )?);
          return finish_loading_imported_module(
            vm,
            scope,
            modules,
            self,
            referrer,
            module_request,
            payload,
            result,
          );
        }
        path
      }
      Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
        let result = Err(module_load_type_error(
          vm,
          scope,
          &format!("module not found: {specifier_utf8}"),
        )?);
        return finish_loading_imported_module(
          vm,
          scope,
          modules,
          self,
          referrer,
          module_request,
          payload,
          result,
        );
      }
      Err(_) => {
        let result = Err(module_load_type_error(
          vm,
          scope,
          &format!("module not found: {specifier_utf8}"),
        )?);
        return finish_loading_imported_module(
          vm,
          scope,
          modules,
          self,
          referrer,
          module_request,
          payload,
          result,
        );
      }
    };

    let key = ModuleCacheKey {
      path: canonical.clone(),
      attributes: module_request.attributes.clone(),
    };
    if let Some(existing) = self.module_cache.get(&key).copied() {
      return finish_loading_imported_module(
        vm,
        scope,
        modules,
        self,
        referrer,
        module_request,
        payload,
        Ok(existing),
      );
    }

    // Avoid platform-dependent IO/UTF-8 error messages: map to deterministic TypeErrors.
    let bytes = match std::fs::read(&canonical) {
      Ok(bytes) => bytes,
      Err(_) => {
        let result = Err(module_load_type_error(
          vm,
          scope,
          &format!("module not found: {specifier_utf8}"),
        )?);
        return finish_loading_imported_module(
          vm,
          scope,
          modules,
          self,
          referrer,
          module_request,
          payload,
          result,
        );
      }
    };

    let source = match String::from_utf8(bytes) {
      Ok(s) => s,
      Err(_) => {
        let result = Err(module_load_type_error(
          vm,
          scope,
          "module source was not valid UTF-8",
        )?);
        return finish_loading_imported_module(
          vm,
          scope,
          modules,
          self,
          referrer,
          module_request,
          payload,
          result,
        );
      }
    };

    // Avoid infallible `Arc<str>` allocation: `SourceText::new_charged_arc` performs fallible
    // allocation internally.
    let source_name = canonical.to_string_lossy();
    let source_text = match kind {
      RequestedModuleKind::JavaScript => {
        SourceText::new_charged_arc(scope.heap_mut(), source_name.as_ref(), source)?
      }
      RequestedModuleKind::Json => {
        let json: JsonValue = match serde_json::from_str(&source) {
          Ok(v) => v,
          Err(err) => {
            let result = Err(module_load_syntax_error_message(
              vm,
              scope,
              &format!("Failed to parse JSON module '{specifier_utf8}': {err}"),
            )?);
            return finish_loading_imported_module(
              vm,
              scope,
              modules,
              self,
              referrer,
              module_request,
              payload,
              result,
            );
          }
        };

        // JSON is a syntactic subset of JavaScript expressions, so `serde_json::to_string` produces
        // a valid module-default-export expression.
        let json_expr = match serde_json::to_string(&json) {
          Ok(s) => s,
          Err(err) => {
            let result = Err(module_load_syntax_error_message(
              vm,
              scope,
              &format!("Failed to serialize JSON module '{specifier_utf8}': {err}"),
            )?);
            return finish_loading_imported_module(
              vm,
              scope,
              modules,
              self,
              referrer,
              module_request,
              payload,
              result,
            );
          }
        };
        let synthesized = format!("export default {json_expr};\n");
        SourceText::new_charged_arc(scope.heap_mut(), source_name.as_ref(), synthesized)?
      }
    };

    let record = match SourceTextModuleRecord::parse_source_with_vm(
      vm,
      scope.heap_mut(),
      Arc::clone(&source_text),
    ) {
      Ok(record) => record,
      Err(VmError::Syntax(mut diags)) => {
        // Preserve parse diagnostics when rejecting the module-loading promise.
        //
        // If we passed `Err(VmError::Syntax(..))` through to `FinishLoadingImportedModule`,
        // `GraphLoadingState::reject_promise` would reject with `undefined` (because
        // `VmError::Syntax` has no `thrown_value()`), losing the error type/message.
        let message =
          render_syntax_diagnostics(&specifier_utf8, source_text.text.as_ref(), &mut diags);
        let intr = vm
          .intrinsics()
          .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
        let err_value = vm_js::new_syntax_error_object(scope, &intr, &message)?;
        scope.push_root(err_value)?;
        let result = Err(VmError::Throw(err_value));
        return finish_loading_imported_module(
          vm,
          scope,
          modules,
          self,
          referrer,
          module_request,
          payload,
          result,
        );
      }
      Err(err @ VmError::Termination(_)) => return Err(err),
      Err(err) => {
        let result = Err(module_load_syntax_error(vm, scope, &err)?);
        return finish_loading_imported_module(
          vm,
          scope,
          modules,
          self,
          referrer,
          module_request,
          payload,
          result,
        );
      }
    };

    let id = modules.add_module(record)?;
    // Cache before finishing so cycles can resolve to the same module record.
    self.register_module_path(id, canonical.clone());
    self.register_module_cache(canonical, module_request.attributes.clone(), id);

    finish_loading_imported_module(
      vm,
      scope,
      modules,
      self,
      referrer,
      module_request,
      payload,
      Ok(id),
    )
  }
}

fn derive_test_root_dir(case_path: &Path) -> PathBuf {
  for ancestor in case_path.ancestors() {
    if ancestor.file_name() == Some(OsStr::new("test")) {
      return ancestor.to_path_buf();
    }
  }
  case_path.parent().unwrap_or(case_path).to_path_buf()
}

fn normalize_path(path: &Path) -> PathBuf {
  let mut out = PathBuf::new();
  for component in path.components() {
    match component {
      Component::CurDir => {}
      Component::ParentDir => {
        let _ = out.pop();
      }
      Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
        out.push(component.as_os_str())
      }
    }
  }
  out
}

fn is_absolute_filesystem_specifier(specifier: &str) -> bool {
  if specifier.starts_with('/') {
    return true;
  }
  // Windows UNC and `\` rooted paths.
  if specifier.starts_with('\\') || specifier.starts_with("//") {
    return true;
  }
  // Windows drive-letter paths.
  let bytes = specifier.as_bytes();
  bytes.len() >= 2 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic()
}

/// A `test262-semantic` executor backed by the `vm-js` interpreter.
#[derive(Debug, Clone, Copy)]
pub struct VmJsExecutor {
  heap_limits: HeapLimits,
}

impl Default for VmJsExecutor {
  fn default() -> Self {
    Self {
      heap_limits: HeapLimits::new(DEFAULT_HEAP_MAX_BYTES, DEFAULT_HEAP_GC_THRESHOLD_BYTES),
    }
  }
}

impl VmJsExecutor {
  fn execute_in_current_thread(
    &self,
    case: &TestCase,
    source: &str,
    cancel: &Arc<AtomicBool>,
  ) -> ExecResult {
    if cancel.load(Ordering::Relaxed) {
      return Err(ExecError::Cancelled);
    }

    let is_async = case.metadata.flags.iter().any(|flag| flag == "async");

    // Keep `max_stack_depth` conservative: the `vm-js` interpreter still uses host recursion
    // heavily enough that very deep call stacks can overflow the native stack before the default
    // `VmOptions::max_stack_depth` guard triggers (even on the enlarged test thread stack).
    //
    // The exact limit is not part of ECMAScript semantics; it is a harness safety boundary.
    let vm = Vm::new(VmOptions {
      interrupt_flag: Some(Arc::clone(cancel)),
      max_stack_depth: 256,
      ..VmOptions::default()
    });
    let heap = Heap::new(self.heap_limits);
    let mut runtime = match vm_js::JsRuntime::new(vm, heap) {
      Ok(runtime) => runtime,
      Err(err) => {
        return Err(ExecError::Js(JsError::new(
          ExecPhase::Runtime,
          None,
          err.to_string(),
        )));
      }
    };

    if let Err(err) = install_test262_host_object(&mut runtime) {
      return Err(ExecError::Js(JsError::new(
        ExecPhase::Runtime,
        None,
        format!("failed to install $262 host object: {err}"),
      )));
    }

    // Dynamic `import()` expression evaluation requires a module graph pointer on the VM, even for
    // classic-script tests (the spec referrer defaults to the current realm).
    {
      let (vm, modules, _heap) = runtime.vm_modules_and_heap_mut();
      vm.set_module_graph(modules);
    }

    // Give the VM a useful/stable source name for stack traces.
    let file_name = if case.id.is_empty() {
      "<test262>".to_string()
    } else {
      case.id.clone()
    };

    if case.variant != Variant::Module {
      if is_async {
        if let Err(err) = install_done_global(&mut runtime) {
          return Err(map_vm_error(case, source, cancel, &mut runtime, err));
        }
      }

      let mut hooks = Test262ModuleHooks::new(&case.path);
      let outcome: ExecResult = (|| {
        let source_text = match SourceText::new_charged_arc(&mut runtime.heap, file_name, source) {
          Ok(source_text) => source_text,
          Err(err) => return Err(map_vm_error(case, source, cancel, &mut runtime, err)),
        };
        let result = runtime.exec_script_source_with_hooks(&mut hooks, source_text);

        match result {
          Ok(_) => {
            // Cancellation should win over successful execution so the runner can surface
            // `timed_out` outcomes deterministically. However, if execution already produced an
            // error (especially stack overflow), we must map that error before checking
            // cancellation so it is not misclassified as a timeout.
            if cancel.load(Ordering::Relaxed) {
              drain_microtasks_into_hooks(&mut runtime, &mut hooks);
              hooks.microtasks.teardown(&mut runtime);
              return Err(ExecError::Cancelled);
            }

            if is_async {
              wait_for_done(case, source, cancel, &mut runtime, &mut hooks)?;
            } else {
              drain_microtasks_into_hooks(&mut runtime, &mut hooks);
              if let Some(err) =
                handle_microtask_errors(case, source, cancel, &mut runtime, &mut hooks)
              {
                return Err(err);
              }
            }
          }
          Err(err) => {
            // Discard queued jobs so persistent roots are cleaned up before dropping the runtime.
            drain_microtasks_into_hooks(&mut runtime, &mut hooks);
            hooks.microtasks.teardown(&mut runtime);
            return Err(map_vm_error(case, source, cancel, &mut runtime, err));
          }
        }

        if cancel.load(Ordering::Relaxed) {
          return Err(ExecError::Cancelled);
        }

        Ok(())
      })();

      teardown_created_realms(&mut runtime, &mut hooks);
      return outcome;
    }

    if is_async {
      if let Err(err) = install_done_global(&mut runtime) {
        return Err(map_vm_error(case, source, cancel, &mut runtime, err));
      }
    }

    execute_module(case, &file_name, source, cancel, is_async, &mut runtime)
  }
}

impl Executor for VmJsExecutor {
  fn execute(&self, case: &TestCase, source: &str, cancel: &Arc<AtomicBool>) -> ExecResult {
    // To avoid aborting the entire process on a host stack overflow, run each test case on a fresh
    // OS thread with an explicit (large) stack.
    //
    // Note: this intentionally happens inside the executor rather than in the runner so we can
    // apply it only to the `vm-js` backend.
    if cancel.load(Ordering::Relaxed) {
      return Err(ExecError::Cancelled);
    }

    // Use scoped threads so we can reuse the caller's `&TestCase` / `&str` without cloning large
    // sources or test bodies.
    thread::scope(|scope| {
      let cancel_for_thread = Arc::clone(cancel);
      let exec = *self;

      let handle = thread::Builder::new()
        .stack_size(TEST_CASE_THREAD_STACK_SIZE)
        .spawn_scoped(scope, move || {
          exec.execute_in_current_thread(case, source, &cancel_for_thread)
        });

      let handle = match handle {
        Ok(handle) => handle,
        Err(err) => {
          // If we're cancelled, preserve existing cancellation semantics.
          if cancel.load(Ordering::Relaxed) {
            return Err(ExecError::Cancelled);
          }
          return Err(ExecError::Js(JsError::new(
            ExecPhase::Runtime,
            None,
            format!("failed to spawn executor thread: {err}"),
          )));
        }
      };

      match handle.join() {
        Ok(result) => result,
        Err(payload) => {
          if cancel.load(Ordering::Relaxed) {
            return Err(ExecError::Cancelled);
          }

          let msg = if let Some(s) = payload.downcast_ref::<&'static str>() {
            (*s).to_string()
          } else if let Some(s) = payload.downcast_ref::<String>() {
            s.clone()
          } else {
            "<non-string panic payload>".to_string()
          };

          Err(ExecError::Js(JsError::new(
            ExecPhase::Runtime,
            None,
            format!("panic while executing test case: {msg}"),
          )))
        }
      }
    })
  }
}

fn data_desc(
  value: Value,
  writable: bool,
  enumerable: bool,
  configurable: bool,
) -> PropertyDescriptor {
  PropertyDescriptor {
    enumerable,
    configurable,
    kind: PropertyKind::Data { value, writable },
  }
}

fn global_data_desc(value: Value) -> PropertyDescriptor {
  data_desc(
    value, /* writable */ true, /* enumerable */ false, /* configurable */ true,
  )
}

fn install_test262_host_object_for_realm(
  vm: &mut Vm,
  scope: &mut vm_js::Scope<'_>,
  realm_id: RealmId,
  global_object: GcObject,
  intr: Intrinsics,
) -> Result<GcObject, VmError> {
  scope.push_root(Value::Object(global_object))?;

  // Register native call handlers.
  //
  // Note: `VmJsExecutor` creates a fresh `Vm` per test case, so registering these handlers on every
  // invocation does not leak ids across tests.
  let create_realm_call = vm.register_native_call(test262_create_realm)?;
  let gc_call = vm.register_native_call(test262_gc)?;
  let detach_array_buffer_call = vm.register_native_call(test262_detach_array_buffer)?;
  let eval_script_call = vm.register_native_call(test262_eval_script)?;

  // Allocate `$262` as a regular object.
  let obj_262 = scope.alloc_object()?;
  scope.push_root(Value::Object(obj_262))?;
  scope
    .heap_mut()
    .object_set_prototype(obj_262, Some(intr.object_prototype()))?;

  // `$262.global = globalThis` (best-effort).
  {
    let key_s = scope.alloc_string("global")?;
    scope.push_root(Value::String(key_s))?;
    scope.define_property(
      obj_262,
      PropertyKey::from_string(key_s),
      data_desc(Value::Object(global_object), true, true, true),
    )?;
  }

  // `$262.IsHTMLDDA` (stubbed to `undefined`).
  {
    let key_s = scope.alloc_string("IsHTMLDDA")?;
    scope.push_root(Value::String(key_s))?;
    scope.define_property(
      obj_262,
      PropertyKey::from_string(key_s),
      data_desc(Value::Undefined, true, true, true),
    )?;
  }

  // Define native methods on `$262`.
  let mut define_native =
    |name: &str, call: vm_js::NativeFunctionId, length: u32| -> Result<(), VmError> {
      let name_s = scope.alloc_string(name)?;
      scope.push_root(Value::String(name_s))?;

      let func = scope.alloc_native_function(call, None, name_s, length)?;
      scope.push_root(Value::Object(func))?;
      scope
        .heap_mut()
        .object_set_prototype(func, Some(intr.function_prototype()))?;
      scope.heap_mut().set_function_realm(func, global_object)?;
      scope.heap_mut().set_function_job_realm(func, realm_id)?;

      scope.define_property(
        obj_262,
        PropertyKey::from_string(name_s),
        data_desc(Value::Object(func), true, true, true),
      )?;
      Ok(())
    };

  define_native("createRealm", create_realm_call, 0)?;
  define_native("gc", gc_call, 0)?;
  define_native("detachArrayBuffer", detach_array_buffer_call, 1)?;
  define_native("evalScript", eval_script_call, 1)?;

  // Define global `$262` binding.
  let key_s = scope.alloc_string("$262")?;
  scope.push_root(Value::String(key_s))?;
  scope.define_property(
    global_object,
    PropertyKey::from_string(key_s),
    global_data_desc(Value::Object(obj_262)),
  )?;

  Ok(obj_262)
}

fn install_test262_host_object(runtime: &mut vm_js::JsRuntime) -> Result<(), VmError> {
  let (vm, realm, heap) = runtime.vm_realm_and_heap_mut();
  let realm_id = realm.id();
  let global_object = realm.global_object();
  let intr = *realm.intrinsics();
  let mut scope = heap.scope();
  let _ = install_test262_host_object_for_realm(vm, &mut scope, realm_id, global_object, intr)?;
  Ok(())
}

fn test262_create_realm(
  vm: &mut Vm,
  scope: &mut vm_js::Scope<'_>,
  _host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  let Some(any) = hooks.as_any_mut() else {
    return Err(VmError::InvariantViolation(
      "$262.createRealm requires host hooks downcasting support",
    ));
  };
  let Some(test262_hooks) = any.downcast_mut::<Test262ModuleHooks>() else {
    return Err(VmError::InvariantViolation(
      "$262.createRealm requires Test262ModuleHooks",
    ));
  };

  let caller_realm = vm.current_realm().ok_or(VmError::InvariantViolation(
    "$262.createRealm requires an active realm",
  ))?;

  // Create the new realm on the same heap/agent (shared symbol registry).
  let mut realm = match Realm::new(vm, scope.heap_mut()) {
    Ok(realm) => realm,
    Err(err) => {
      return Err(err);
    }
  };

  // Install `$262` on the new realm's global object.
  let realm_id = realm.id();
  let global_object = realm.global_object();
  let intr = *realm.intrinsics();
  let obj_262 =
    match install_test262_host_object_for_realm(vm, scope, realm_id, global_object, intr) {
      Ok(obj) => obj,
      Err(err) => {
        realm.teardown(scope.heap_mut());
        vm.teardown_realm(scope.heap_mut(), realm_id);
        let _ = vm.load_realm_state(scope.heap_mut(), caller_realm);
        return Err(err);
      }
    };
  scope.push_root(Value::Object(obj_262))?;

  // Store the realm so its persistent roots can be torn down after the test completes.
  if test262_hooks.created_realms.try_reserve(1).is_err() {
    let realm_id = realm.id();
    realm.teardown(scope.heap_mut());
    vm.teardown_realm(scope.heap_mut(), realm_id);
    let _ = vm.load_realm_state(scope.heap_mut(), caller_realm);
    return Err(VmError::OutOfMemory);
  }
  test262_hooks.created_realms.push(realm);

  // Restore the calling realm as the active realm state.
  let _ = vm.load_realm_state(scope.heap_mut(), caller_realm)?;

  Ok(Value::Object(obj_262))
}

fn test262_gc(
  _vm: &mut Vm,
  scope: &mut vm_js::Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  _args: &[Value],
) -> Result<Value, VmError> {
  scope.heap_mut().collect_garbage();
  Ok(Value::Undefined)
}

fn test262_detach_array_buffer(
  _vm: &mut Vm,
  scope: &mut vm_js::Scope<'_>,
  _host: &mut dyn VmHost,
  _hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let arg0 = args.get(0).copied().unwrap_or(Value::Undefined);
  let Value::Object(obj) = arg0 else {
    return Err(VmError::TypeError(
      "$262.detachArrayBuffer requires an ArrayBuffer object",
    ));
  };
  if !scope.heap().is_array_buffer_object(obj) {
    return Err(VmError::TypeError(
      "$262.detachArrayBuffer requires an ArrayBuffer object",
    ));
  }
  scope.heap_mut().detach_array_buffer(obj)?;
  Ok(Value::Undefined)
}

fn test262_eval_script(
  vm: &mut Vm,
  scope: &mut vm_js::Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let source_val = args.get(0).copied().unwrap_or(Value::Undefined);
  let source = scope.to_string(vm, host, hooks, source_val)?;
  vm_js::eval_script_with_host_and_hooks(vm, scope, host, hooks, source)
}

fn execute_module(
  case: &TestCase,
  file_name: &str,
  source: &str,
  cancel: &Arc<AtomicBool>,
  is_async: bool,
  runtime: &mut vm_js::JsRuntime,
) -> ExecResult {
  let (harness_src, module_src) = split_module_source(source);

  let mut hooks = Test262ModuleHooks::new(&case.path);
  let result: ExecResult = (|| {
    // 1) Run the harness prelude as a classic script to populate the global object.
    if !harness_src.trim().is_empty() {
      let harness_name = format!("{file_name}#harness");
      let harness_source =
        match SourceText::new_charged_arc(&mut runtime.heap, harness_name, harness_src) {
          Ok(source) => source,
          Err(err) => return Err(map_vm_error(case, harness_src, cancel, runtime, err)),
        };
      let result = runtime.exec_script_source_with_hooks(&mut hooks, harness_source);

      if let Err(err) = result {
        return Err(map_vm_error(case, harness_src, cancel, runtime, err));
      }

      if cancel.load(Ordering::Relaxed) {
        return Err(ExecError::Cancelled);
      }

      drain_microtasks_into_hooks(runtime, &mut hooks);
      if let Some(err) = handle_microtask_errors(case, source, cancel, runtime, &mut hooks) {
        return Err(err);
      }
    }

    // 2) Parse the module source text.
    let module_source = match SourceText::new_charged_arc(&mut runtime.heap, file_name, module_src)
    {
      Ok(source) => source,
      Err(err) => return Err(map_vm_error(case, module_src, cancel, runtime, err)),
    };
    let record = match SourceTextModuleRecord::parse_source_with_vm(
      &mut runtime.vm,
      &mut runtime.heap,
      module_source,
    ) {
      Ok(record) => record,
      Err(err) => return Err(map_vm_error(case, module_src, cancel, runtime, err)),
    };

    let module_id = match runtime.modules_mut().add_module(record) {
      Ok(id) => id,
      Err(err) => return Err(map_vm_error(case, module_src, cancel, runtime, err)),
    };

    // Record path metadata for relative import resolution (and cache the root module for cycles).
    let root_path = match std::fs::canonicalize(&case.path) {
      Ok(p) => p,
      Err(_) => case.path.clone(),
    };
    hooks.register_module_path(module_id, root_path.clone());
    hooks.register_module_cache(root_path, Vec::new(), module_id);

    // 3) Load requested (static) modules.
    let load_promise = {
      // Do not map errors to `ExecError` while holding a `Scope` borrow of `runtime.heap`.
      let result: Result<Value, VmError> = {
        let (vm, modules, heap) = runtime.vm_modules_and_heap_mut();
        let mut scope = heap.scope();
        vm_js::load_requested_modules(
          vm,
          &mut scope,
          modules,
          &mut hooks,
          module_id,
          HostDefined::default(),
        )
      };
      match result {
        Ok(v) => v,
        Err(err) => {
          return Err(map_vm_error_with_phase(
            case,
            module_src,
            cancel,
            runtime,
            ExecPhase::Resolution,
            err,
          ))
        }
      }
    };

    let Value::Object(load_promise_obj) = load_promise else {
      return Err(ExecError::Js(JsError::new(
        ExecPhase::Resolution,
        None,
        "LoadRequestedModules returned a non-object promise",
      )));
    };

    let load_promise_root = add_persistent_root(
      case,
      module_src,
      cancel,
      runtime,
      ExecPhase::Resolution,
      Value::Object(load_promise_obj),
    )?;

    let load_outcome: ExecResult = (|| {
      if cancel.load(Ordering::Relaxed) {
        // Cancellation normally wins so the runner can surface timeouts deterministically.
        //
        // However, if the module-loading promise has already settled with a stack overflow, report
        // that as a JS-visible RangeError instead of `Cancelled` so stack overflow is not
        // misclassified as a timeout.
        if let Ok(PromiseState::Rejected) = runtime.heap.promise_state(load_promise_obj) {
          if let Ok(reason) = runtime.heap.promise_result(load_promise_obj) {
            let reason = reason.unwrap_or(Value::Undefined);
            let (typ, message, stack) = describe_thrown_value_with_stack(runtime, reason);
            if matches!(typ.as_deref(), Some("RangeError") | Some("Error"))
              && is_stack_overflow_message(&message)
            {
              return Err(ExecError::Js(JsError {
                phase: ExecPhase::Resolution,
                typ: Some("RangeError".to_string()),
                message,
                stack,
              }));
            }
          }
        }
        return Err(ExecError::Cancelled);
      }

      drain_microtasks_into_hooks(runtime, &mut hooks);
      if let Some(err) = handle_microtask_errors(case, source, cancel, runtime, &mut hooks) {
        return Err(err);
      }

      match runtime.heap.promise_state(load_promise_obj) {
        Ok(PromiseState::Fulfilled) => Ok(()),
        Ok(PromiseState::Rejected) => {
          let reason = runtime
            .heap
            .promise_result(load_promise_obj)
            .map_err(|err| {
              map_vm_error_with_phase(
                case,
                module_src,
                cancel,
                runtime,
                ExecPhase::Resolution,
                err,
              )
            })?
            .unwrap_or(Value::Undefined);
          let (typ, message, stack) = describe_thrown_value_with_stack(runtime, reason);
          Err(ExecError::Js(JsError {
            phase: ExecPhase::Resolution,
            typ,
            message,
            stack,
          }))
        }
        Ok(PromiseState::Pending) => Err(ExecError::Js(JsError::new(
          ExecPhase::Resolution,
          None,
          "module loading promise remained pending after microtask checkpoint",
        ))),
        Err(err) => Err(map_vm_error_with_phase(
          case,
          module_src,
          cancel,
          runtime,
          ExecPhase::Resolution,
          err,
        )),
      }
    })();

    runtime.heap.remove_root(load_promise_root);
    load_outcome?;

    // 3.5) Link the module graph (instantiation). This ensures link-time failures are reported as
    // `negative.phase: resolution` in test262.
    {
      let global_object = runtime.realm().global_object();
      let realm_id = runtime.realm().id();
      let link_result: Result<(), VmError> = {
        let (vm, modules, heap) = runtime.vm_modules_and_heap_mut();
        modules.link(vm, heap, global_object, realm_id, module_id)
      };
      if let Err(err) = link_result {
        return Err(map_vm_error_with_phase(
          case,
          module_src,
          cancel,
          runtime,
          ExecPhase::Resolution,
          err,
        ));
      }
    }

    // 4) Evaluate the root module (promise-returning API).
    let eval_promise = {
      let global_object = runtime.realm().global_object();
      let realm_id = runtime.realm().id();

      // Avoid mapping to `ExecError` while holding the borrow-split `(&mut Vm, &mut ModuleGraph, &mut Heap)`.
      let eval_result: Result<Value, VmError> = {
        let (vm, modules, heap) = runtime.vm_modules_and_heap_mut();
        let mut dummy_host = ();
        modules.evaluate(
          vm,
          heap,
          global_object,
          realm_id,
          module_id,
          &mut dummy_host,
          &mut hooks,
        )
      };

      match eval_result {
        Ok(v) => v,
        Err(err) => {
          return Err(map_vm_error_with_phase(
            case,
            module_src,
            cancel,
            runtime,
            ExecPhase::Resolution,
            err,
          ))
        }
      }
    };

    let Value::Object(eval_promise_obj) = eval_promise else {
      return Err(ExecError::Js(JsError::new(
        ExecPhase::Runtime,
        None,
        "module evaluation did not return a promise object",
      )));
    };

    let eval_promise_root = add_persistent_root(
      case,
      module_src,
      cancel,
      runtime,
      ExecPhase::Runtime,
      Value::Object(eval_promise_obj),
    )?;

    let eval_outcome: ExecResult = (|| {
      if cancel.load(Ordering::Relaxed) {
        // Cancellation normally wins so the runner can surface timeouts deterministically.
        //
        // However, if the module-evaluation promise has already settled with a stack overflow,
        // report that as a JS-visible RangeError instead of `Cancelled` so stack overflow is not
        // misclassified as a timeout.
        if let Ok(PromiseState::Rejected) = runtime.heap.promise_state(eval_promise_obj) {
          if let Ok(reason) = runtime.heap.promise_result(eval_promise_obj) {
            let reason = reason.unwrap_or(Value::Undefined);
            let (typ, message, stack) = describe_thrown_value_with_stack(runtime, reason);
            if matches!(typ.as_deref(), Some("RangeError") | Some("Error"))
              && is_stack_overflow_message(&message)
            {
              return Err(ExecError::Js(JsError {
                phase: ExecPhase::Runtime,
                typ: Some("RangeError".to_string()),
                message,
                stack,
              }));
            }
          }
        }
        return Err(ExecError::Cancelled);
      }

      drain_microtasks_into_hooks(runtime, &mut hooks);
      if let Some(err) = handle_microtask_errors(case, source, cancel, runtime, &mut hooks) {
        return Err(err);
      }

      match runtime.heap.promise_state(eval_promise_obj) {
        Ok(PromiseState::Fulfilled) => Ok(()),
        Ok(PromiseState::Rejected) => {
          let reason = runtime
            .heap
            .promise_result(eval_promise_obj)
            .map_err(|err| {
              map_vm_error_with_phase(case, module_src, cancel, runtime, ExecPhase::Runtime, err)
            })?
            .unwrap_or(Value::Undefined);
          let (typ, message, stack) = describe_thrown_value_with_stack(runtime, reason);
          Err(ExecError::Js(JsError {
            phase: ExecPhase::Runtime,
            typ,
            message,
            stack,
          }))
        }
        Ok(PromiseState::Pending) => {
          let (vm, modules, heap) = runtime.vm_modules_and_heap_mut();
          modules.abort_tla_evaluation(vm, heap, module_id);
          Err(ExecError::Js(JsError::new(
            ExecPhase::Runtime,
            None,
            "module evaluation promise remained pending after microtask checkpoint",
          )))
        }
        Err(err) => Err(map_vm_error_with_phase(
          case,
          module_src,
          cancel,
          runtime,
          ExecPhase::Runtime,
          err,
        )),
      }
    })();

    runtime.heap.remove_root(eval_promise_root);
    eval_outcome?;

    if is_async {
      wait_for_done(case, source, cancel, runtime, &mut hooks)?;
    }

    Ok(())
  })();

  if result.is_err() {
    // Dropping `Job` values with live persistent roots would trip debug assertions (and leak roots
    // in release builds). Clean up any queued work before returning early.
    drain_microtasks_into_hooks(runtime, &mut hooks);
    hooks.microtasks.teardown(runtime);
  }

  teardown_created_realms(runtime, &mut hooks);

  result
}

fn split_module_source(source: &str) -> (&str, &str) {
  source
    .split_once(MODULE_SEPARATOR_MARKER)
    .map(|(h, m)| (h, m))
    .unwrap_or(("", source))
}

fn drain_microtasks_into_hooks(runtime: &mut vm_js::JsRuntime, hooks: &mut Test262ModuleHooks) {
  while let Some((realm, job)) = runtime.vm.microtask_queue_mut().pop_front() {
    hooks.host_enqueue_promise_job(job, realm);
  }
}

fn teardown_created_realms(runtime: &mut vm_js::JsRuntime, hooks: &mut Test262ModuleHooks) {
  for realm in hooks.created_realms.iter_mut() {
    let realm_id = realm.id();
    realm.teardown(&mut runtime.heap);
    runtime.vm.teardown_realm(&mut runtime.heap, realm_id);
  }
  hooks.created_realms.clear();
}

fn add_persistent_root(
  case: &TestCase,
  source: &str,
  cancel: &Arc<AtomicBool>,
  runtime: &mut vm_js::JsRuntime,
  phase: ExecPhase,
  value: Value,
) -> Result<RootId, ExecError> {
  let result: Result<RootId, VmError> = (|| {
    let mut scope = runtime.heap.scope();
    scope.push_root(value)?;
    scope.heap_mut().add_root(value)
  })();

  result.map_err(|err| map_vm_error_with_phase(case, source, cancel, runtime, phase, err))
}

fn handle_microtask_errors(
  case: &TestCase,
  source: &str,
  cancel: &Arc<AtomicBool>,
  runtime: &mut vm_js::JsRuntime,
  hooks: &mut Test262ModuleHooks,
) -> Option<ExecError> {
  let errors = hooks.perform_microtask_checkpoint(runtime);
  if errors.is_empty() {
    return None;
  }

  // Stack overflow should never be reported as a timeout/cancellation, even if the cancel flag is
  // set (e.g. due to a race with the cooperative timeout).
  for err in errors.iter() {
    let is_stack_overflow = match err {
      VmError::Termination(term) if matches!(term.reason, TerminationReason::StackOverflow) => true,
      VmError::RangeError(message) => is_stack_overflow_message(message),
      VmError::Throw(thrown) => {
        let (typ, message) = describe_thrown_value(runtime, *thrown);
        matches!(typ.as_deref(), Some("RangeError") | Some("Error"))
          && is_stack_overflow_message(&message)
      }
      VmError::ThrowWithStack { value: thrown, .. } => {
        let (typ, message) = describe_thrown_value(runtime, *thrown);
        matches!(typ.as_deref(), Some("RangeError") | Some("Error"))
          && is_stack_overflow_message(&message)
      }
      _ => false,
    };
    if is_stack_overflow {
      return Some(map_vm_error_with_phase(
        case,
        source,
        cancel,
        runtime,
        ExecPhase::Runtime,
        err.clone(),
      ));
    }
  }

  // Cancellation should win for other failures.
  if cancel.load(Ordering::Relaxed) {
    return Some(ExecError::Cancelled);
  }

  // If a job hard-terminated (deadline exceeded, interrupt, etc.), map that directly so the runner
  // can classify it deterministically (most termination reasons map to `Cancelled`).
  if let Some(err) = errors.iter().find(|e| matches!(e, VmError::Termination(_))) {
    return Some(map_vm_error_with_phase(
      case,
      source,
      cancel,
      runtime,
      ExecPhase::Runtime,
      err.clone(),
    ));
  }

  // Treat the first job error as a runtime failure.
  Some(map_vm_error_with_phase(
    case,
    source,
    cancel,
    runtime,
    ExecPhase::Runtime,
    errors[0].clone(),
  ))
}

fn wait_for_done(
  case: &TestCase,
  source: &str,
  cancel: &Arc<AtomicBool>,
  runtime: &mut vm_js::JsRuntime,
  hooks: &mut Test262ModuleHooks,
) -> ExecResult {
  // Ensure we always clear any queued jobs, even on early exit.
  let discard_remaining_jobs = |runtime: &mut vm_js::JsRuntime, hooks: &mut Test262ModuleHooks| {
    drain_microtasks_into_hooks(runtime, hooks);
    hooks.microtasks.teardown(runtime);
  };

  let started = hooks.microtasks.begin_checkpoint();
  // We should never re-enter microtask processing from this executor.
  if !started {
    discard_remaining_jobs(runtime, hooks);
    return Err(ExecError::Js(JsError::new(
      ExecPhase::Runtime,
      None,
      "microtask checkpoint already in progress while waiting for $DONE",
    )));
  }

  let outcome: ExecResult = loop {
    if cancel.load(Ordering::Relaxed) {
      break Err(ExecError::Cancelled);
    }

    let Some(state) = runtime.vm.user_data::<AsyncDoneState>() else {
      break Err(ExecError::Js(JsError::new(
        ExecPhase::Runtime,
        None,
        "$DONE state missing on VM",
      )));
    };
    if state.is_complete() {
      break match &state.error {
        None => Ok(()),
        Some(err) => Err(ExecError::Js(JsError {
          phase: ExecPhase::Runtime,
          typ: err.typ.clone(),
          message: err.message.clone(),
          stack: None,
        })),
      };
    }

    // A safety net: some `vm-js` module-loading operations still temporarily route Promise jobs
    // through the VM-owned queue (e.g. dummy-host helper APIs). Drain it into our hooks so jobs can
    // run with `Test262ModuleHooks` (which implements module loading for dynamic `import()`).
    drain_microtasks_into_hooks(runtime, hooks);

    let Some((_realm, job)) = hooks.microtasks.pop_front() else {
      break Err(ExecError::Js(JsError::new(
        ExecPhase::Runtime,
        None,
        "async test did not call $DONE",
      )));
    };

    if let Err(err) = job.run(runtime, hooks) {
      break Err(map_vm_error_with_phase(
        case,
        source,
        cancel,
        runtime,
        ExecPhase::Runtime,
        err,
      ));
    }
  };

  hooks.microtasks.end_checkpoint();
  discard_remaining_jobs(runtime, hooks);

  outcome
}

fn module_load_type_error(
  vm: &mut Vm,
  scope: &mut vm_js::Scope<'_>,
  message: &str,
) -> Result<VmError, VmError> {
  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
  let value = vm_js::new_type_error_object(scope, &intr, message)?;
  Ok(VmError::Throw(value))
}

fn module_load_syntax_error(
  vm: &mut Vm,
  scope: &mut vm_js::Scope<'_>,
  err: &VmError,
) -> Result<VmError, VmError> {
  module_load_syntax_error_message(vm, scope, &err.to_string())
}

fn module_load_syntax_error_message(
  vm: &mut Vm,
  scope: &mut vm_js::Scope<'_>,
  message: &str,
) -> Result<VmError, VmError> {
  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
  let value = vm_js::new_syntax_error_object(scope, &intr, message)?;
  Ok(VmError::Throw(value))
}

fn describe_thrown_value_with_stack(
  runtime: &mut vm_js::JsRuntime,
  value: Value,
) -> (Option<String>, String, Option<String>) {
  match value {
    Value::Object(obj) => {
      // Root the thrown value while we allocate property keys so GC cannot collect
      // it out from under us.
      let mut scope = runtime.heap.scope();
      let _ = scope.push_root(value);

      let typ = get_object_string_data_property(&mut scope, obj, "name")
        .or_else(|| get_object_constructor_name(&mut scope, obj));
      let message = get_object_string_data_property(&mut scope, obj, "message")
        .or_else(|| typ.clone())
        .unwrap_or_else(|| "<object>".to_string());
      let stack =
        get_object_string_data_property(&mut scope, obj, "stack").filter(|s| !s.is_empty());
      (typ, message, stack)
    }
    other => {
      let (typ, msg) = describe_thrown_value(runtime, other);
      (typ, msg, None)
    }
  }
}

fn is_stack_overflow_message(message: &str) -> bool {
  let message_lc = message.to_ascii_lowercase();
  message_lc.contains("call stack") || message_lc.contains("stack overflow")
}

fn map_vm_error_with_phase(
  case: &TestCase,
  source: &str,
  cancel: &Arc<AtomicBool>,
  runtime: &mut vm_js::JsRuntime,
  phase: ExecPhase,
  err: VmError,
) -> ExecError {
  match err {
    // Stack overflow should never be reported as a timeout/cancellation, even if the cancel flag is
    // set (e.g. due to a race with the cooperative timeout).
    VmError::Termination(term) if matches!(term.reason, TerminationReason::StackOverflow) => {
      ExecError::Js(JsError {
        phase,
        typ: Some("RangeError".to_string()),
        message: term.to_string(),
        stack: stack_from_frames(term.stack),
      })
    }

    // Exceeding `VmOptions::max_stack_depth` is surfaced as a JS `RangeError` (via
    // `coerce_error_to_throw_with_stack` at host boundaries). Ensure it is never misclassified as a
    // timeout/cancellation, even if the cancel flag is set.
    VmError::RangeError(message) if is_stack_overflow_message(message) => ExecError::Js(JsError {
      phase,
      typ: Some("RangeError".to_string()),
      message: format!("range error: {message}"),
      stack: stack_from_frames(runtime.vm.capture_stack()),
    }),
    VmError::Throw(thrown) => {
      let (typ, message, stack) = describe_thrown_value_with_stack(runtime, thrown);
      if matches!(typ.as_deref(), Some("RangeError") | Some("Error"))
        && is_stack_overflow_message(&message)
      {
        return ExecError::Js(JsError {
          phase,
          typ: Some("RangeError".to_string()),
          message,
          stack,
        });
      }
      if cancel.load(Ordering::Relaxed) {
        return ExecError::Cancelled;
      }
      ExecError::Js(JsError {
        phase,
        typ,
        message,
        stack,
      })
    }
    VmError::ThrowWithStack {
      value: thrown,
      stack,
    } => {
      let (typ, message, _) = describe_thrown_value_with_stack(runtime, thrown);
      if matches!(typ.as_deref(), Some("RangeError") | Some("Error"))
        && is_stack_overflow_message(&message)
      {
        return ExecError::Js(JsError {
          phase,
          typ: Some("RangeError".to_string()),
          message,
          stack: stack_from_frames(stack),
        });
      }
      if cancel.load(Ordering::Relaxed) {
        return ExecError::Cancelled;
      }
      ExecError::Js(JsError {
        phase,
        typ,
        message,
        stack: stack_from_frames(stack),
      })
    }

    // Treat cancellation/timeout as higher priority than other failures so the runner can surface
    // a `timed_out` outcome deterministically.
    _ if cancel.load(Ordering::Relaxed) => ExecError::Cancelled,
    other => {
      let mapped = map_vm_error(case, source, cancel, runtime, other);
      if let ExecError::Js(mut js) = mapped {
        js.phase = phase;
        ExecError::Js(js)
      } else {
        mapped
      }
    }
  }
}

fn map_vm_error(
  case: &TestCase,
  source: &str,
  cancel: &Arc<AtomicBool>,
  runtime: &mut vm_js::JsRuntime,
  err: VmError,
) -> ExecError {
  match err {
    // Stack overflow should never be reported as a timeout/cancellation, even if the cancel flag is
    // set (e.g. due to a race with the cooperative timeout).
    VmError::Termination(term) if matches!(term.reason, TerminationReason::StackOverflow) => {
      ExecError::Js(JsError {
        phase: ExecPhase::Runtime,
        typ: Some("RangeError".to_string()),
        message: term.to_string(),
        stack: stack_from_frames(term.stack),
      })
    }

    // Exceeding `VmOptions::max_stack_depth` is surfaced as a JS `RangeError` (via
    // `coerce_error_to_throw_with_stack` at host boundaries). Ensure it is never misclassified as a
    // timeout/cancellation, even if the cancel flag is set.
    VmError::RangeError(message) if is_stack_overflow_message(message) => ExecError::Js(JsError {
      phase: ExecPhase::Runtime,
      typ: Some("RangeError".to_string()),
      message: format!("range error: {message}"),
      stack: stack_from_frames(runtime.vm.capture_stack()),
    }),
    VmError::Throw(thrown) => {
      let (typ, message) = describe_thrown_value(runtime, thrown);
      if matches!(typ.as_deref(), Some("RangeError") | Some("Error"))
        && is_stack_overflow_message(&message)
      {
        return ExecError::Js(JsError {
          phase: ExecPhase::Runtime,
          typ: Some("RangeError".to_string()),
          message,
          stack: stack_from_frames(runtime.vm.capture_stack()),
        });
      }
      if cancel.load(Ordering::Relaxed) {
        return ExecError::Cancelled;
      }
      let stack = stack_from_frames(runtime.vm.capture_stack());
      ExecError::Js(JsError {
        phase: ExecPhase::Runtime,
        typ,
        message,
        stack,
      })
    }
    VmError::ThrowWithStack {
      value: thrown,
      stack,
    } => {
      let (typ, message) = describe_thrown_value(runtime, thrown);
      if matches!(typ.as_deref(), Some("RangeError") | Some("Error"))
        && is_stack_overflow_message(&message)
      {
        return ExecError::Js(JsError {
          phase: ExecPhase::Runtime,
          typ: Some("RangeError".to_string()),
          message,
          stack: stack_from_frames(stack),
        });
      }
      if cancel.load(Ordering::Relaxed) {
        return ExecError::Cancelled;
      }
      let stack = stack_from_frames(stack);
      ExecError::Js(JsError {
        phase: ExecPhase::Runtime,
        typ,
        message,
        stack,
      })
    }

    // Treat cancellation/timeout as higher priority than other failures so the runner can surface
    // a `timed_out` outcome deterministically.
    _ if cancel.load(Ordering::Relaxed) => ExecError::Cancelled,

    VmError::Syntax(mut diags) => {
      let file_name = if case.id.is_empty() {
        "<test262>"
      } else {
        case.id.as_str()
      };
      let message = render_syntax_diagnostics(file_name, source, &mut diags);

      ExecError::Js(JsError::new(
        ExecPhase::Parse,
        Some("SyntaxError".to_string()),
        message,
      ))
    }

    VmError::Termination(term) => match term.reason {
      TerminationReason::Interrupted
      | TerminationReason::DeadlineExceeded
      | TerminationReason::OutOfFuel => ExecError::Cancelled,

      // `vm-js` normally surfaces call-stack exhaustion as a JS-level `RangeError` via
      // `VmOptions::max_stack_depth`. However, older versions (or unexpected internal paths) may
      // still report a hard termination with `TerminationReason::StackOverflow`.
      //
      // This match arm is a fallback for completeness. The main stack overflow handling happens
      // before the cancellation short-circuit above so stack overflow is never misclassified as a
      // timeout.
      TerminationReason::StackOverflow => ExecError::Js(JsError {
        phase: ExecPhase::Runtime,
        typ: Some("RangeError".to_string()),
        message: term.to_string(),
        stack: stack_from_frames(term.stack),
      }),

      // Chosen mapping: treat OOM as a `RangeError` (resource exhaustion), which
      // is also where we classify stack overflow.
      TerminationReason::OutOfMemory => ExecError::Js(JsError {
        phase: ExecPhase::Runtime,
        typ: Some("RangeError".to_string()),
        message: term.to_string(),
        stack: stack_from_frames(term.stack),
      }),
    },

    VmError::NotCallable
    | VmError::NotConstructable
    | VmError::PrototypeCycle
    | VmError::PropertyNotData
    | VmError::PropertyNotFound
    | VmError::TypeError(_) => ExecError::Js(JsError {
      phase: ExecPhase::Runtime,
      typ: Some("TypeError".to_string()),
      message: err.to_string(),
      stack: stack_from_frames(runtime.vm.capture_stack()),
    }),

    VmError::PrototypeChainTooDeep | VmError::RangeError(_) => ExecError::Js(JsError {
      phase: ExecPhase::Runtime,
      typ: Some("RangeError".to_string()),
      message: err.to_string(),
      stack: stack_from_frames(runtime.vm.capture_stack()),
    }),

    // Chosen mapping: treat OOM as a `RangeError` (resource exhaustion), which
    // is also where we classify stack overflow.
    VmError::OutOfMemory => ExecError::Js(JsError {
      phase: ExecPhase::Runtime,
      typ: Some("RangeError".to_string()),
      message: err.to_string(),
      stack: stack_from_frames(runtime.vm.capture_stack()),
    }),

    VmError::Unimplemented(_) => ExecError::Js(JsError {
      phase: ExecPhase::Runtime,
      typ: None,
      message: err.to_string(),
      stack: stack_from_frames(runtime.vm.capture_stack()),
    }),

    other => ExecError::Js(JsError {
      phase: ExecPhase::Runtime,
      typ: None,
      message: other.to_string(),
      stack: stack_from_frames(runtime.vm.capture_stack()),
    }),
  }
}

fn render_syntax_diagnostics(
  file_name: &str,
  source: &str,
  diags: &mut Vec<diagnostics::Diagnostic>,
) -> String {
  diagnostics::sort_diagnostics(diags);

  let mut files = SimpleFiles::new();
  let _ = files.add(file_name, source);

  diags
    .iter()
    .map(|d| render_diagnostic(&files, d).trim_end().to_string())
    .collect::<Vec<_>>()
    .join("\n\n")
}

fn describe_thrown_value(runtime: &mut vm_js::JsRuntime, value: Value) -> (Option<String>, String) {
  // Root the thrown value while we allocate property keys so GC cannot collect
  // it out from under us.
  let mut scope = runtime.heap.scope();
  let _ = scope.push_root(value);

  match value {
    Value::Object(obj) => {
      let typ = get_object_string_data_property(&mut scope, obj, "name")
        .or_else(|| get_object_constructor_name(&mut scope, obj));
      let message = get_object_string_data_property(&mut scope, obj, "message")
        .or_else(|| typ.clone())
        .unwrap_or_else(|| "<object>".to_string());
      (typ, message)
    }

    Value::Undefined => (None, "undefined".to_string()),
    Value::Null => (None, "null".to_string()),
    Value::Bool(b) => (None, b.to_string()),
    Value::Number(n) => (None, format_js_number(n)),
    Value::BigInt(b) => {
      let msg = scope
        .heap()
        .get_bigint(b)
        .ok()
        .and_then(|bi| bi.to_string_radix_with_tick(10, &mut || Ok(())).ok())
        .unwrap_or_else(|| "<bigint>".to_string());
      (None, msg)
    }
    Value::String(s) => {
      let msg = scope
        .heap()
        .get_string(s)
        .map(|s| s.to_utf8_lossy())
        .unwrap_or_else(|_| "<string>".to_string());
      (None, msg)
    }
    Value::Symbol(sym) => {
      let msg = scope
        .heap()
        .symbol_description(sym)
        .and_then(|desc| {
          scope
            .heap()
            .get_string(desc)
            .ok()
            .map(|s| s.to_utf8_lossy())
        })
        .map(|desc| format!("Symbol({desc})"))
        .unwrap_or_else(|| "Symbol()".to_string());
      (None, msg)
    }
  }
}

fn get_object_string_data_property(
  scope: &mut vm_js::Scope<'_>,
  obj: vm_js::GcObject,
  prop: &str,
) -> Option<String> {
  let key = PropertyKey::from_string(scope.alloc_string(prop).ok()?);
  let desc = scope.heap().get_property(obj, &key).ok().flatten()?;
  match desc.kind {
    PropertyKind::Data { value, .. } => match value {
      Value::String(s) => scope.heap().get_string(s).ok().map(|s| s.to_utf8_lossy()),
      _ => None,
    },
    PropertyKind::Accessor { .. } => None,
  }
}

fn get_object_data_property(
  scope: &mut vm_js::Scope<'_>,
  obj: vm_js::GcObject,
  prop: &str,
) -> Option<Value> {
  let key = PropertyKey::from_string(scope.alloc_string(prop).ok()?);
  let desc = scope.heap().get_property(obj, &key).ok().flatten()?;
  match desc.kind {
    PropertyKind::Data { value, .. } => Some(value),
    PropertyKind::Accessor { .. } => None,
  }
}

fn get_object_constructor_name(
  scope: &mut vm_js::Scope<'_>,
  obj: vm_js::GcObject,
) -> Option<String> {
  let ctor = get_object_data_property(scope, obj, "constructor")?;
  let Value::Object(ctor_obj) = ctor else {
    return None;
  };
  get_object_string_data_property(scope, ctor_obj, "name")
}

fn format_js_number(n: f64) -> String {
  if n.is_nan() {
    return "NaN".to_string();
  }
  if n.is_infinite() {
    return if n.is_sign_negative() {
      "-Infinity".to_string()
    } else {
      "Infinity".to_string()
    };
  }
  // Best-effort: Rust's formatting matches JS for the common cases we care
  // about (`1`, `-0`, etc).
  n.to_string()
}

fn stack_from_frames(frames: Vec<StackFrame>) -> Option<String> {
  if frames.is_empty() {
    return None;
  }
  let formatted = format_stack_trace(&frames);
  if formatted.is_empty() {
    None
  } else {
    Some(formatted)
  }
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::frontmatter::Frontmatter;
  use crate::harness::{assemble_source, HarnessMode};
  use crate::report::ExpectedOutcome;
  use std::fs;
  use std::path::PathBuf;
  use tempfile::tempdir;

  fn test_case(id: &str) -> TestCase {
    TestCase {
      id: id.to_string(),
      path: PathBuf::from(id),
      variant: Variant::NonStrict,
      expected: ExpectedOutcome::Pass,
      metadata: Frontmatter::default(),
      body: String::new(),
    }
  }

  #[test]
  fn cancellation_flag_short_circuits() {
    let exec = VmJsExecutor::default();
    let cancel = Arc::new(AtomicBool::new(true));
    let err = exec
      .execute(&test_case("cancel.js"), "1;", &cancel)
      .unwrap_err();
    assert!(matches!(err, ExecError::Cancelled));
  }

  #[test]
  fn syntax_error_maps_to_parse_syntaxerror() {
    let exec = VmJsExecutor::default();
    let cancel = Arc::new(AtomicBool::new(false));
    let err = exec
      .execute(&test_case("syntax.js"), "let =;", &cancel)
      .unwrap_err();
    let ExecError::Js(js) = err else {
      panic!("expected JS error, got {err:?}");
    };
    assert_eq!(js.phase, ExecPhase::Parse);
    assert_eq!(js.typ.as_deref(), Some("SyntaxError"));
    assert!(
      js.message.contains("syntax.js"),
      "rendered diagnostic should include file name, got: {}",
      js.message
    );
  }

  #[test]
  fn throw_number_maps_to_runtime_error() {
    let exec = VmJsExecutor::default();
    let cancel = Arc::new(AtomicBool::new(false));
    let err = exec
      .execute(&test_case("throw.js"), "throw 1;", &cancel)
      .unwrap_err();
    let ExecError::Js(js) = err else {
      panic!("expected JS error, got {err:?}");
    };
    assert_eq!(js.phase, ExecPhase::Runtime);
    assert!(js.typ.is_none());
    assert_eq!(js.message, "1");
  }

  #[test]
  fn deep_recursion_maps_to_stack_overflow_rangeerror() {
    let exec = VmJsExecutor::default();
    let cancel = Arc::new(AtomicBool::new(false));
    let err = exec
      .execute(
        &test_case("stack_overflow.js"),
        r#"
function f(n) {
  if (n === 0) return 0;
  // Not a tail call; should always grow the call stack even if PTC is implemented.
  return 1 + f(n - 1);
}
f(2000);
"#,
        &cancel,
      )
      .unwrap_err();
    let ExecError::Js(js) = err else {
      panic!("expected JS error, got {err:?}");
    };
    assert_eq!(js.phase, ExecPhase::Runtime);
    assert_eq!(js.typ.as_deref(), Some("RangeError"));
    // Depending on where the VM detects the overflow, this may come from:
    // - the VM's own max-stack-depth guard ("Maximum call stack size exceeded"), or
    // - a hard termination converted into a RangeError ("execution terminated: stack overflow").
    let message_lc = js.message.to_ascii_lowercase();
    assert!(
      message_lc.contains("call stack") || message_lc.contains("stack overflow"),
      "expected stack overflow message, got: {}",
      js.message
    );
  }

  #[test]
  fn termination_stack_overflow_maps_to_rangeerror_not_cancelled() {
    let case = test_case("termination_stack_overflow.js");

    // Even if the cancellation flag is set, stack overflow should still map to a JS-visible
    // RangeError (not `ExecError::Cancelled`, which the runner reports as a timeout).
    for cancelled in [false, true] {
      let cancel = Arc::new(AtomicBool::new(cancelled));

      let vm = Vm::new(VmOptions::default());
      let heap = Heap::new(HeapLimits::new(
        DEFAULT_HEAP_MAX_BYTES,
        DEFAULT_HEAP_GC_THRESHOLD_BYTES,
      ));
      let mut runtime = vm_js::JsRuntime::new(vm, heap).expect("init runtime");

      let term = vm_js::Termination::new(TerminationReason::StackOverflow, Vec::new());
      let err = map_vm_error(&case, "", &cancel, &mut runtime, VmError::Termination(term));
      let ExecError::Js(js) = err else {
        panic!("expected JS error, got {err:?} (cancelled={cancelled})");
      };
      assert_eq!(js.phase, ExecPhase::Runtime);
      assert_eq!(js.typ.as_deref(), Some("RangeError"));
      assert!(
        js.message.to_ascii_lowercase().contains("stack overflow"),
        "expected stack overflow message, got: {}",
        js.message
      );
    }
  }

  #[test]
  fn termination_stack_overflow_maps_to_rangeerror_with_phase_even_when_cancelled() {
    let case = test_case("termination_stack_overflow_phase.js");

    // `map_vm_error_with_phase` is used by module execution paths; ensure it preserves the
    // requested phase and still maps stack overflow to a JS RangeError even if the cancel flag is
    // set.
    for cancelled in [false, true] {
      let cancel = Arc::new(AtomicBool::new(cancelled));

      let vm = Vm::new(VmOptions::default());
      let heap = Heap::new(HeapLimits::new(
        DEFAULT_HEAP_MAX_BYTES,
        DEFAULT_HEAP_GC_THRESHOLD_BYTES,
      ));
      let mut runtime = vm_js::JsRuntime::new(vm, heap).expect("init runtime");

      let term = vm_js::Termination::new(TerminationReason::StackOverflow, Vec::new());
      let err = map_vm_error_with_phase(
        &case,
        "",
        &cancel,
        &mut runtime,
        ExecPhase::Resolution,
        VmError::Termination(term),
      );
      let ExecError::Js(js) = err else {
        panic!("expected JS error, got {err:?} (cancelled={cancelled})");
      };
      assert_eq!(js.phase, ExecPhase::Resolution);
      assert_eq!(js.typ.as_deref(), Some("RangeError"));
      assert!(
        js.message.to_ascii_lowercase().contains("stack overflow"),
        "expected stack overflow message, got: {}",
        js.message
      );
    }
  }

  #[test]
  fn microtask_termination_stack_overflow_maps_to_rangeerror_even_when_cancelled() {
    let case = test_case("microtask_termination_stack_overflow.js");
    let source = "// dummy source";

    for cancelled in [false, true] {
      let cancel = Arc::new(AtomicBool::new(cancelled));

      let vm = Vm::new(VmOptions::default());
      let heap = Heap::new(HeapLimits::new(
        DEFAULT_HEAP_MAX_BYTES,
        DEFAULT_HEAP_GC_THRESHOLD_BYTES,
      ));
      let mut runtime = vm_js::JsRuntime::new(vm, heap).expect("init runtime");

      let path = PathBuf::from("test/microtask_termination_stack_overflow.js");
      let mut hooks = Test262ModuleHooks::new(&path);

      let term = vm_js::Termination::new(TerminationReason::StackOverflow, Vec::new());
      let job = Job::new(vm_js::JobKind::Promise, move |_ctx, _host| {
        Err(VmError::Termination(term))
      })
      .expect("alloc job");

      // Enqueue directly onto the host's microtask queue and run a checkpoint via
      // `handle_microtask_errors`.
      hooks.host_enqueue_promise_job(job, None);

      let err = handle_microtask_errors(&case, source, &cancel, &mut runtime, &mut hooks)
        .unwrap_or_else(|| panic!("expected microtask error (cancelled={cancelled})"));

      let ExecError::Js(js) = err else {
        panic!("expected JS error, got {err:?} (cancelled={cancelled})");
      };
      assert_eq!(js.phase, ExecPhase::Runtime);
      assert_eq!(js.typ.as_deref(), Some("RangeError"));
      assert!(
        js.message.to_ascii_lowercase().contains("stack overflow"),
        "expected stack overflow message, got: {}",
        js.message
      );
    }
  }

  #[test]
  fn thrown_stack_overflow_maps_to_rangeerror_even_when_cancelled() {
    let case = test_case("thrown_stack_overflow.js");

    for cancelled in [false, true] {
      let cancel = Arc::new(AtomicBool::new(cancelled));

      let vm = Vm::new(VmOptions::default());
      let heap = Heap::new(HeapLimits::new(
        DEFAULT_HEAP_MAX_BYTES,
        DEFAULT_HEAP_GC_THRESHOLD_BYTES,
      ));
      let mut runtime = vm_js::JsRuntime::new(vm, heap).expect("init runtime");

      let intr = runtime.vm.intrinsics().expect("intrinsics initialized");
      let (thrown, root): (Value, RootId) = {
        let mut scope = runtime.heap.scope();
        let value =
          vm_js::new_range_error(&mut scope, intr, "Maximum call stack size exceeded").unwrap();
        let _ = scope.push_root(value);
        let root = scope.heap_mut().add_root(value).expect("add root");
        (value, root)
      };

      let err = map_vm_error(
        &case,
        "",
        &cancel,
        &mut runtime,
        VmError::ThrowWithStack {
          value: thrown,
          stack: Vec::new(),
        },
      );
      let ExecError::Js(js) = err else {
        panic!("expected JS error, got {err:?} (cancelled={cancelled})");
      };
      assert_eq!(js.phase, ExecPhase::Runtime);
      assert_eq!(js.typ.as_deref(), Some("RangeError"));
      assert!(
        js.message.to_ascii_lowercase().contains("call stack")
          || js.message.to_ascii_lowercase().contains("stack overflow"),
        "expected stack overflow message, got: {} (cancelled={cancelled})",
        js.message
      );

      // Ensure `map_vm_error_with_phase` also preserves stack overflow when cancelled.
      let err = map_vm_error_with_phase(
        &case,
        "",
        &cancel,
        &mut runtime,
        ExecPhase::Resolution,
        VmError::ThrowWithStack {
          value: thrown,
          stack: Vec::new(),
        },
      );
      let ExecError::Js(js) = err else {
        panic!("expected JS error, got {err:?} (cancelled={cancelled})");
      };
      assert_eq!(js.phase, ExecPhase::Resolution);
      assert_eq!(js.typ.as_deref(), Some("RangeError"));
      assert!(
        js.message.to_ascii_lowercase().contains("call stack")
          || js.message.to_ascii_lowercase().contains("stack overflow"),
        "expected stack overflow message, got: {} (cancelled={cancelled})",
        js.message
      );

      runtime.heap.remove_root(root);
    }
  }

  #[test]
  fn microtask_range_error_stack_overflow_maps_to_rangeerror_even_when_cancelled() {
    let case = test_case("microtask_range_error_stack_overflow.js");
    let source = "// dummy source";

    for cancelled in [false, true] {
      let cancel = Arc::new(AtomicBool::new(cancelled));

      let vm = Vm::new(VmOptions::default());
      let heap = Heap::new(HeapLimits::new(
        DEFAULT_HEAP_MAX_BYTES,
        DEFAULT_HEAP_GC_THRESHOLD_BYTES,
      ));
      let mut runtime = vm_js::JsRuntime::new(vm, heap).expect("init runtime");

      let path = PathBuf::from("test/microtask_range_error_stack_overflow.js");
      let mut hooks = Test262ModuleHooks::new(&path);

      let job = Job::new(vm_js::JobKind::Promise, move |_ctx, _host| {
        Err(VmError::RangeError("Maximum call stack size exceeded"))
      })
      .expect("alloc job");

      hooks.host_enqueue_promise_job(job, None);

      let err = handle_microtask_errors(&case, source, &cancel, &mut runtime, &mut hooks)
        .unwrap_or_else(|| panic!("expected microtask error (cancelled={cancelled})"));

      let ExecError::Js(js) = err else {
        panic!("expected JS error, got {err:?} (cancelled={cancelled})");
      };
      assert_eq!(js.phase, ExecPhase::Runtime);
      assert_eq!(js.typ.as_deref(), Some("RangeError"));
      assert!(
        js.message.to_ascii_lowercase().contains("call stack")
          || js.message.to_ascii_lowercase().contains("stack overflow"),
        "expected stack overflow message, got: {} (cancelled={cancelled})",
        js.message
      );
    }
  }

  #[test]
  fn range_error_variant_maps_to_rangeerror_type() {
    let case = test_case("range_error_variant.js");
    for cancelled in [false, true] {
      let cancel = Arc::new(AtomicBool::new(cancelled));

      let vm = Vm::new(VmOptions::default());
      let heap = Heap::new(HeapLimits::new(
        DEFAULT_HEAP_MAX_BYTES,
        DEFAULT_HEAP_GC_THRESHOLD_BYTES,
      ));
      let mut runtime = vm_js::JsRuntime::new(vm, heap).expect("init runtime");

      let err = map_vm_error(
        &case,
        "",
        &cancel,
        &mut runtime,
        VmError::RangeError("Maximum call stack size exceeded"),
      );
      let ExecError::Js(js) = err else {
        panic!("expected JS error, got {err:?} (cancelled={cancelled})");
      };
      assert_eq!(js.phase, ExecPhase::Runtime);
      assert_eq!(js.typ.as_deref(), Some("RangeError"));
      assert!(
        js.message.to_ascii_lowercase().contains("call stack"),
        "expected call stack message, got: {} (cancelled={cancelled})",
        js.message
      );
    }
  }

  #[test]
  fn eval_script_creates_global_lexical_bindings() {
    let exec = VmJsExecutor::default();
    let cancel = Arc::new(AtomicBool::new(false));
    exec
      .execute(
        &test_case("eval_script_lexical.js"),
        r#"
var assert = {};
assert.sameValue = function (actual, expected, message) {
  if (actual !== expected) {
    throw new Error(message || ('assert.sameValue failed: expected ' + expected + ', got ' + actual));
  }
};

// Global lexical declarations must be created even if the global object is non-extensible.
Object.preventExtensions(this);

$262.evalScript('let test262let = 1;');
test262let = 2;
assert.sameValue(test262let, 2, '`let` binding is mutable');
assert.sameValue(this.hasOwnProperty('test262let'), false, 'let does not create a global property');
"#,
        &cancel,
      )
      .expect("expected $262.evalScript to execute as global script");
  }

  #[test]
  fn create_realm_creates_fresh_intrinsics_and_shared_symbol_registry() {
    let exec = VmJsExecutor::default();
    let cancel = Arc::new(AtomicBool::new(false));
    exec
      .execute(
        &test_case("create_realm.js"),
        r#"
var assert = {};
assert.sameValue = function (actual, expected, message) {
  if (actual !== expected) {
    throw new Error(message || ('assert.sameValue failed: expected ' + expected + ', got ' + actual));
  }
};

var realm = $262.createRealm();
var other = realm.global;
assert.sameValue(other.$262, realm, 'new realm should install and return its own $262 object');

assert.sameValue(other.Object !== Object, true, 'Object constructor differs across realms');
assert.sameValue(other.Symbol.for('x'), Symbol.for('x'), 'Symbol registry is shared across realms');
assert.sameValue(other.Symbol.iterator, Symbol.iterator, 'well-known symbols are shared across realms');

// `$262.evalScript` must run in the realm it is associated with.
realm.evalScript('globalThis.__evalScript_ran_in_new_realm = 1;');
assert.sameValue(other.__evalScript_ran_in_new_realm, 1, 'evalScript executes in created realm');
assert.sameValue(typeof __evalScript_ran_in_new_realm, 'undefined', 'evalScript does not pollute the caller realm');

var obj = Reflect.construct(Object, [], other.Function);
assert.sameValue(Object.getPrototypeOf(obj) === other.Function.prototype, true, 'Reflect.construct uses newTarget prototype');

// `$262.createRealm` must work recursively from the new realm.
var realm2 = realm.createRealm();
assert.sameValue(realm2.global.Object !== other.Object, true, 'createRealm is recursive');
"#,
        &cancel,
      )
      .expect("expected $262.createRealm to create a fresh Realm");
  }

  #[test]
  fn create_realm_teardown_clears_vm_owned_state() -> Result<(), VmError> {
    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(
      DEFAULT_HEAP_MAX_BYTES,
      DEFAULT_HEAP_GC_THRESHOLD_BYTES,
    ));
    let mut runtime = vm_js::JsRuntime::new(vm, heap)?;
    install_test262_host_object(&mut runtime)?;

    // Baseline roots for the main realm/runtime.
    let baseline_roots = runtime.heap.persistent_root_count();

    let mut hooks = Test262ModuleHooks::new(Path::new("test/create_realm_teardown.js"));

    // Create a new realm, then run a tagged template literal inside that realm so the VM populates
    // its per-realm template registry (`GetTemplateObject` cache) with a persistent root.
    let source = r#"
      (function () {
        var realm = $262.createRealm();
        realm.evalScript("function tag(s) { return s; } tag`hello`;");
      })();
    "#;
    let source_text =
      SourceText::new_charged_arc(&mut runtime.heap, "create_realm_teardown.js", source)?;
    runtime.exec_script_source_with_hooks(&mut hooks, source_text)?;

    assert!(
      !hooks.created_realms.is_empty(),
      "expected $262.createRealm to record the created realm for teardown"
    );
    let created_realm_id = hooks.created_realms[0].id();

    // The created realm (and the VM template registry entry) should add new persistent roots.
    assert!(
      runtime.heap.persistent_root_count() > baseline_roots,
      "expected created realm to allocate persistent roots"
    );

    // Teardown must clear both heap-owned realm roots and VM-owned per-realm state.
    teardown_created_realms(&mut runtime, &mut hooks);
    assert_eq!(
      runtime.heap.persistent_root_count(),
      baseline_roots,
      "expected $262.createRealm teardown to restore baseline persistent roots"
    );

    // The VM should not allow switching to a torn-down realm.
    let err = runtime
      .vm
      .load_realm_state(&mut runtime.heap, created_realm_id)
      .expect_err("expected load_realm_state to fail after realm teardown");
    assert!(matches!(
      err,
      VmError::InvariantViolation("unknown realm id")
    ));

    Ok(())
  }

  #[test]
  fn module_without_separator_executes_entire_source_as_module() {
    let exec = VmJsExecutor::default();
    let cancel = Arc::new(AtomicBool::new(false));
    let mut case = test_case("module_no_sep.js");
    case.variant = Variant::Module;
    exec
      .execute(&case, "export const x = 1;\n", &cancel)
      .expect("module should execute");
  }

  fn setup_test262_with_assert() -> tempfile::TempDir {
    let temp = tempdir().unwrap();
    fs::create_dir_all(temp.path().join("harness")).unwrap();
    fs::write(
      temp.path().join("harness/assert.js"),
      r#"
var assert = {
  sameValue(actual, expected) {
    if (actual !== expected) {
      throw new Error("Assertion failed: " + actual + " !== " + expected);
    }
  }
};
"#,
    )
    .unwrap();
    fs::write(temp.path().join("harness/sta.js"), "").unwrap();
    fs::create_dir_all(temp.path().join("test")).unwrap();
    temp
  }

  #[test]
  fn module_variant_supports_imports_and_harness_globals() {
    let test262 = setup_test262_with_assert();

    let test_dir = test262.path().join("test");
    let main_path = test_dir.join("main.js");
    let dep_path = test_dir.join("dep.js");
    fs::write(&main_path, "/* placeholder */").unwrap();
    fs::write(&dep_path, "export const x = 1;\n").unwrap();

    let case = TestCase {
      id: "main.js".to_string(),
      path: main_path.clone(),
      variant: Variant::Module,
      expected: ExpectedOutcome::Pass,
      metadata: Frontmatter::default(),
      body: "import { x } from './dep.js';\nassert.sameValue(x, 1);\n".to_string(),
    };

    let source = assemble_source(
      test262.path(),
      &case.metadata,
      case.variant,
      &case.body,
      HarnessMode::Test262,
    )
    .unwrap();

    let exec = VmJsExecutor::default();
    let cancel = Arc::new(AtomicBool::new(false));
    exec.execute(&case, &source, &cancel).unwrap();
  }

  #[test]
  fn module_variant_throw_maps_to_runtime_error() {
    let test262 = setup_test262_with_assert();
    let test_dir = test262.path().join("test");
    let main_path = test_dir.join("throw.js");
    fs::write(&main_path, "/* placeholder */").unwrap();

    let case = TestCase {
      id: "throw.js".to_string(),
      path: main_path.clone(),
      variant: Variant::Module,
      expected: ExpectedOutcome::Pass,
      metadata: Frontmatter::default(),
      body: "throw new TypeError('boom');\n".to_string(),
    };

    let source = assemble_source(
      test262.path(),
      &case.metadata,
      case.variant,
      &case.body,
      HarnessMode::Test262,
    )
    .unwrap();

    let exec = VmJsExecutor::default();
    let cancel = Arc::new(AtomicBool::new(false));
    let err = exec.execute(&case, &source, &cancel).unwrap_err();
    let ExecError::Js(js) = err else {
      panic!("expected JS error, got {err:?}");
    };
    assert_eq!(js.phase, ExecPhase::Runtime);
    assert_eq!(js.typ.as_deref(), Some("TypeError"));
    assert_eq!(js.message, "boom");
  }

  #[test]
  fn module_variant_async_requires_done() {
    let test262 = setup_test262_with_assert();
    let test_dir = test262.path().join("test");
    let main_path = test_dir.join("async_missing_done.js");
    fs::write(&main_path, "/* placeholder */").unwrap();

    let case = TestCase {
      id: "async_missing_done.js".to_string(),
      path: main_path,
      variant: Variant::Module,
      expected: ExpectedOutcome::Pass,
      metadata: Frontmatter {
        flags: vec!["async".to_string(), "module".to_string()],
        ..Frontmatter::default()
      },
      body: "export const x = 1;\n".to_string(),
    };

    let source = assemble_source(
      test262.path(),
      &case.metadata,
      case.variant,
      &case.body,
      HarnessMode::Test262,
    )
    .unwrap();

    let exec = VmJsExecutor::default();
    let cancel = Arc::new(AtomicBool::new(false));
    let err = exec.execute(&case, &source, &cancel).unwrap_err();
    let ExecError::Js(js) = err else {
      panic!("expected JS error, got {err:?}");
    };
    assert_eq!(js.phase, ExecPhase::Runtime);
    assert!(
      js.message.contains("async test did not call $DONE"),
      "unexpected error message: {}",
      js.message
    );
  }

  #[test]
  fn module_variant_async_done_allows_success() {
    let test262 = setup_test262_with_assert();
    let test_dir = test262.path().join("test");
    let main_path = test_dir.join("async_done.js");
    fs::write(&main_path, "/* placeholder */").unwrap();

    let case = TestCase {
      id: "async_done.js".to_string(),
      path: main_path,
      variant: Variant::Module,
      expected: ExpectedOutcome::Pass,
      metadata: Frontmatter {
        flags: vec!["async".to_string(), "module".to_string()],
        ..Frontmatter::default()
      },
      body: "Promise.resolve().then(() => $DONE());\nexport const x = 1;\n".to_string(),
    };

    let source = assemble_source(
      test262.path(),
      &case.metadata,
      case.variant,
      &case.body,
      HarnessMode::Test262,
    )
    .unwrap();

    let exec = VmJsExecutor::default();
    let cancel = Arc::new(AtomicBool::new(false));
    exec.execute(&case, &source, &cancel).unwrap();
  }

  #[test]
  fn module_variant_missing_import_maps_to_resolution_error() {
    let test262 = setup_test262_with_assert();
    let test_dir = test262.path().join("test");
    let main_path = test_dir.join("missing_import.js");
    fs::write(&main_path, "/* placeholder */").unwrap();

    let case = TestCase {
      id: "missing_import.js".to_string(),
      path: main_path.clone(),
      variant: Variant::Module,
      expected: ExpectedOutcome::Pass,
      metadata: Frontmatter::default(),
      body: "import './no_such_module.js';\n".to_string(),
    };

    let source = assemble_source(
      test262.path(),
      &case.metadata,
      case.variant,
      &case.body,
      HarnessMode::Test262,
    )
    .unwrap();

    let exec = VmJsExecutor::default();
    let cancel = Arc::new(AtomicBool::new(false));
    let err = exec.execute(&case, &source, &cancel).unwrap_err();
    let ExecError::Js(js) = err else {
      panic!("expected JS error, got {err:?}");
    };
    assert_eq!(js.phase, ExecPhase::Resolution);
    assert_eq!(js.typ.as_deref(), Some("TypeError"));
    assert!(
      js.message.contains("no_such_module.js"),
      "error message should mention missing specifier, got: {}",
      js.message
    );
  }

  #[test]
  fn microtask_checkpoint_runs_jobs_with_module_loading_hooks_for_dynamic_import() {
    let temp = tempdir().unwrap();
    let test_dir = temp.path().join("test");
    fs::create_dir_all(&test_dir).unwrap();
    let test_path = test_dir.join("case.js");
    fs::write(&test_path, "/* placeholder */").unwrap();
    fs::write(test_dir.join("dep.js"), "export const x = 1;\n").unwrap();

    let vm = Vm::new(VmOptions::default());
    let heap = Heap::new(HeapLimits::new(
      DEFAULT_HEAP_MAX_BYTES,
      DEFAULT_HEAP_GC_THRESHOLD_BYTES,
    ));
    let mut runtime = vm_js::JsRuntime::new(vm, heap).unwrap();
    let mut hooks = Test262ModuleHooks::new(&test_path);

    let source = r#"
      globalThis.__import_ok = false;
      Promise.resolve()
        .then(() => import("./dep.js"))
        .then(
          ns => { globalThis.__import_ok = (ns.x === 1); },
          err => { throw err; }
        );
    "#;

    let source_text = SourceText::new_charged_arc(&mut runtime.heap, "case.js", source).unwrap();
    runtime
      .exec_script_source_with_hooks(&mut hooks, source_text)
      .unwrap();

    // Drain the host-owned queue and ensure the Promise job runs with `Test262ModuleHooks` as the
    // host hook implementation (so dynamic `import()` inside the callback can resolve modules).
    drain_microtasks_into_hooks(&mut runtime, &mut hooks);
    let errors = hooks.perform_microtask_checkpoint(&mut runtime);
    if !errors.is_empty() {
      hooks.microtasks.teardown(&mut runtime);
      panic!("unexpected microtask errors: {errors:?}");
    }

    assert!(
      !hooks.module_cache.is_empty(),
      "expected dynamic import to invoke the module loader"
    );

    let global = runtime.realm().global_object();
    let mut scope = runtime.heap.scope();
    let key = PropertyKey::from_string(scope.alloc_string("__import_ok").unwrap());
    let value = scope.heap().get(global, &key).unwrap();
    assert_eq!(value, Value::Bool(true));
  }

  #[test]
  fn script_variant_drains_microtasks_and_supports_dynamic_import_in_promise_jobs() {
    let temp = tempdir().unwrap();
    let test_dir = temp.path().join("test");
    fs::create_dir_all(&test_dir).unwrap();
    let main_path = test_dir.join("main.js");
    fs::write(&main_path, "/* placeholder */").unwrap();
    fs::write(test_dir.join("dep.js"), "export const x = 1;\n").unwrap();

    let case = TestCase {
      id: "main.js".to_string(),
      path: main_path,
      variant: Variant::NonStrict,
      expected: ExpectedOutcome::Pass,
      metadata: Frontmatter::default(),
      body: String::new(),
    };

    let source = r#"
      Promise.resolve()
        .then(() => import("./dep.js"))
        .then(ns => {
          if (ns.x !== 1) throw new Error("bad export");
        });
    "#;

    let exec = VmJsExecutor::default();
    let cancel = Arc::new(AtomicBool::new(false));
    exec.execute(&case, source, &cancel).unwrap();
  }

  #[test]
  fn module_variant_harness_error_teardowns_pending_microtasks() {
    let test262 = tempdir().unwrap();
    fs::create_dir_all(test262.path().join("harness")).unwrap();
    // Schedule a Promise job (which holds persistent roots) and then throw, ensuring the executor
    // tears down queued jobs before returning the error.
    fs::write(
      test262.path().join("harness/assert.js"),
      r#"
Promise.resolve().then(() => {});
throw new Error("boom");
"#,
    )
    .unwrap();
    fs::write(test262.path().join("harness/sta.js"), "").unwrap();
    fs::create_dir_all(test262.path().join("test")).unwrap();

    let test_dir = test262.path().join("test");
    let main_path = test_dir.join("harness_throw.js");
    fs::write(&main_path, "/* placeholder */").unwrap();

    let case = TestCase {
      id: "harness_throw.js".to_string(),
      path: main_path,
      variant: Variant::Module,
      expected: ExpectedOutcome::Pass,
      metadata: Frontmatter::default(),
      body: "export const x = 1;\n".to_string(),
    };

    let source = assemble_source(
      test262.path(),
      &case.metadata,
      case.variant,
      &case.body,
      HarnessMode::Test262,
    )
    .unwrap();

    let exec = VmJsExecutor::default();
    let cancel = Arc::new(AtomicBool::new(false));
    let err = exec.execute(&case, &source, &cancel).unwrap_err();
    let ExecError::Js(js) = err else {
      panic!("expected JS error, got {err:?}");
    };
    assert_eq!(js.phase, ExecPhase::Runtime);
    assert_eq!(js.typ.as_deref(), Some("Error"));
    assert_eq!(js.message, "boom");
  }
}
