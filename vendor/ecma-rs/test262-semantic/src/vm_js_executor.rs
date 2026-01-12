use crate::executor::{ExecError, ExecPhase, ExecResult, Executor, JsError};
use crate::harness::MODULE_SEPARATOR_MARKER;
use crate::report::Variant;
use crate::runner::TestCase;
use diagnostics::render::render_diagnostic;
use diagnostics::SimpleFiles;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use vm_js::format_stack_trace;
use vm_js::{
  finish_loading_imported_module, HostDefined, ImportAttribute, Job, ModuleGraph, ModuleId, ModuleLoadPayload,
  ModuleReferrer, ModuleRequest, PromiseState, RealmId, RootId, VmHostHooks, VmJobContext,
};
use vm_js::{
  Heap, HeapLimits, MicrotaskQueue, PropertyKey, PropertyKind, SourceText, SourceTextModuleRecord,
  StackFrame, TerminationReason, Value, Vm, VmError, VmOptions,
};
  
const DEFAULT_HEAP_MAX_BYTES: usize = 64 * 1024 * 1024;
const DEFAULT_HEAP_GC_THRESHOLD_BYTES: usize = 32 * 1024 * 1024;

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
  /// Directory used to resolve `ModuleReferrer::Realm` (dynamic import from classic scripts).
  test_dir: PathBuf,
  module_paths: HashMap<ModuleId, PathBuf>,
  module_cache: HashMap<ModuleCacheKey, ModuleId>,
}

impl Test262ModuleHooks {
  fn new(test_path: &Path) -> Self {
    Self {
      microtasks: MicrotaskQueue::new(),
      test_dir: test_path.parent().unwrap_or_else(|| Path::new("")).to_path_buf(),
      module_paths: HashMap::new(),
      module_cache: HashMap::new(),
    }
  }

  fn register_module_path(&mut self, id: ModuleId, path: PathBuf) {
    self.module_paths.insert(id, path);
  }

  fn register_module_cache(&mut self, path: PathBuf, attributes: Vec<ImportAttribute>, id: ModuleId) {
    self.module_cache.insert(ModuleCacheKey { path, attributes }, id);
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
      ModuleReferrer::Script(_) => Err(VmError::Unimplemented(
        "module loading from Script referrer (no ScriptId->path mapping)",
      )),
    }
  }

  fn perform_microtask_checkpoint(
    &mut self,
    ctx: &mut dyn VmJobContext,
  ) -> Vec<VmError> {
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

  fn as_any_mut(&mut self) -> Option<&mut dyn std::any::Any> {
    Some(self)
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

    let candidate = base_dir.join(&specifier);
    let canonical = match std::fs::canonicalize(&candidate) {
      Ok(p) => p,
      Err(_) => {
        let result = Err(module_load_type_error(
          vm,
          scope,
          &format!("Cannot find module '{specifier}'"),
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

    let source_text = match std::fs::read_to_string(&canonical) {
      Ok(src) => Arc::new(SourceText::new(canonical.to_string_lossy().into_owned(), src)),
      Err(err) => {
        let result = Err(module_load_type_error(
          vm,
          scope,
          &format!("Failed to read module '{}': {err}", canonical.display()),
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

    let record = match SourceTextModuleRecord::parse_source_with_vm(vm, source_text) {
      Ok(record) => record,
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

    let id = modules.add_module(record);
    // Cache before finishing so cycles can resolve to the same module record.
    self.register_module_path(id, canonical.clone());
    self.register_module_cache(canonical, module_request.attributes.clone(), id);

    finish_loading_imported_module(vm, scope, modules, self, referrer, module_request, payload, Ok(id))
  }
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
 
impl Executor for VmJsExecutor {
  fn execute(&self, case: &TestCase, source: &str, cancel: &Arc<AtomicBool>) -> ExecResult {
    if cancel.load(Ordering::Relaxed) {
      return Err(ExecError::Cancelled);
    }

    let vm = Vm::new(VmOptions {
      interrupt_flag: Some(Arc::clone(cancel)),
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
 
    // Give the VM a useful/stable source name for stack traces.
    let file_name = if case.id.is_empty() {
      "<test262>".to_string()
    } else {
      case.id.clone()
    };

    if case.variant != Variant::Module {
      let source_text = Arc::new(SourceText::new(file_name, source));
      let result = runtime.exec_script_source(source_text);

      // Cancellation should win over any other outcome (including parse/runtime errors).
      if cancel.load(Ordering::Relaxed) {
        return Err(ExecError::Cancelled);
      }

      return match result {
        Ok(_) => Ok(()),
        Err(err) => Err(map_vm_error(case, source, cancel, &mut runtime, err)),
      };
    }

    execute_module(case, &file_name, source, cancel, &mut runtime)
  }
}

fn execute_module(
  case: &TestCase,
  file_name: &str,
  source: &str,
  cancel: &Arc<AtomicBool>,
  runtime: &mut vm_js::JsRuntime,
) -> ExecResult {
  let (harness_src, module_src) = split_module_source(source);

  let mut hooks = Test262ModuleHooks::new(&case.path);

  // 1) Run the harness prelude as a classic script to populate the global object.
  if !harness_src.trim().is_empty() {
    let harness_name = format!("{file_name}#harness");
    let harness_source = Arc::new(SourceText::new(harness_name, harness_src));
    let result = runtime.exec_script_source_with_hooks(&mut hooks, harness_source);

    if cancel.load(Ordering::Relaxed) {
      return Err(ExecError::Cancelled);
    }

    if let Err(err) = result {
      return Err(map_vm_error(case, harness_src, cancel, runtime, err));
    }

    drain_microtasks_into_hooks(runtime, &mut hooks);
    if let Some(err) = handle_microtask_errors(case, source, cancel, runtime, &mut hooks) {
      return Err(err);
    }
  }

  // 2) Parse the module source text.
  let module_source = Arc::new(SourceText::new(file_name.to_string(), module_src));
  let record = match SourceTextModuleRecord::parse_source_with_vm(&mut runtime.vm, module_source) {
    Ok(record) => record,
    Err(err) => return Err(map_vm_error(case, module_src, cancel, runtime, err)),
  };

  let module_id = runtime.modules_mut().add_module(record);

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
      vm_js::load_requested_modules(vm, &mut scope, modules, &mut hooks, module_id, HostDefined::default())
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
            map_vm_error_with_phase(case, module_src, cancel, runtime, ExecPhase::Resolution, err)
          })?
          .unwrap_or(Value::Undefined);
        let (typ, message, stack) = describe_thrown_value_with_stack(runtime, reason);
        let phase = match typ.as_deref() {
          Some("SyntaxError") => ExecPhase::Parse,
          _ => ExecPhase::Resolution,
        };
        Err(ExecError::Js(JsError {
          phase,
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

  // 4) Evaluate the root module (promise-returning API).
  let eval_promise = {
    let global_object = runtime.realm().global_object();
    let realm_id = runtime.realm().id();

    // Avoid mapping to `ExecError` while holding the borrow-split `(&mut Vm, &mut ModuleGraph, &mut Heap)`.
    let eval_result: Result<Value, VmError> = {
      let (vm, modules, heap) = runtime.vm_modules_and_heap_mut();
      let mut dummy_host = ();
      modules.evaluate(vm, heap, global_object, realm_id, module_id, &mut dummy_host, &mut hooks)
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
  eval_outcome
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

  // Cancellation should win.
  if cancel.load(Ordering::Relaxed) {
    return Some(ExecError::Cancelled);
  }

  if errors.iter().any(|e| matches!(e, VmError::Termination(_))) {
    return Some(ExecError::Cancelled);
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

fn module_load_type_error(vm: &mut Vm, scope: &mut vm_js::Scope<'_>, message: &str) -> Result<VmError, VmError> {
  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
  let value = vm_js::new_type_error_object(scope, &intr, message)?;
  Ok(VmError::Throw(value))
}

fn module_load_syntax_error(vm: &mut Vm, scope: &mut vm_js::Scope<'_>, err: &VmError) -> Result<VmError, VmError> {
  let intr = vm
    .intrinsics()
    .ok_or(VmError::Unimplemented("intrinsics not initialized"))?;
  let message = err.to_string();
  let value = vm_js::new_syntax_error_object(scope, &intr, &message)?;
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

      let typ = get_object_string_data_property(&mut scope, obj, "name");
      let message = get_object_string_data_property(&mut scope, obj, "message")
        .or_else(|| typ.clone())
        .unwrap_or_else(|| "<object>".to_string());
      let stack = get_object_string_data_property(&mut scope, obj, "stack").filter(|s| !s.is_empty());
      (typ, message, stack)
    }
    other => {
      let (typ, msg) = describe_thrown_value(runtime, other);
      (typ, msg, None)
    }
  }
}

fn map_vm_error_with_phase(
  case: &TestCase,
  source: &str,
  cancel: &Arc<AtomicBool>,
  runtime: &mut vm_js::JsRuntime,
  phase: ExecPhase,
  err: VmError,
) -> ExecError {
  if cancel.load(Ordering::Relaxed) {
    return ExecError::Cancelled;
  }

  match err {
    VmError::Throw(thrown) => {
      let (typ, message, stack) = describe_thrown_value_with_stack(runtime, thrown);
      ExecError::Js(JsError {
        phase,
        typ,
        message,
        stack,
      })
    }
    VmError::ThrowWithStack { value: thrown, stack } => {
      let (typ, message, _) = describe_thrown_value_with_stack(runtime, thrown);
      ExecError::Js(JsError {
        phase,
        typ,
        message,
        stack: stack_from_frames(stack),
      })
    }
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
  if cancel.load(Ordering::Relaxed) {
    return ExecError::Cancelled;
  }
 
  match err {
    VmError::Syntax(mut diags) => {
      diagnostics::sort_diagnostics(&mut diags);
 
      let file_name = if case.id.is_empty() {
        "<test262>".to_string()
      } else {
        case.id.clone()
      };
      let mut files = SimpleFiles::new();
      let _ = files.add(file_name, source);
 
      let message = diags
        .iter()
        .map(|d| render_diagnostic(&files, d).trim_end().to_string())
        .collect::<Vec<_>>()
        .join("\n\n");
 
      ExecError::Js(JsError::new(
        ExecPhase::Parse,
        Some("SyntaxError".to_string()),
        message,
      ))
    }
 
    VmError::Throw(thrown) => {
      let (typ, message) = describe_thrown_value(runtime, thrown);
      let stack = stack_from_frames(runtime.vm.capture_stack());
      ExecError::Js(JsError {
        phase: ExecPhase::Runtime,
        typ,
        message,
        stack,
      })
    }

    VmError::ThrowWithStack { value: thrown, stack } => {
      let (typ, message) = describe_thrown_value(runtime, thrown);
      let stack = stack_from_frames(stack);
      ExecError::Js(JsError {
        phase: ExecPhase::Runtime,
        typ,
        message,
        stack,
      })
    }
 
    VmError::Termination(term) => match term.reason {
      TerminationReason::Interrupted | TerminationReason::DeadlineExceeded | TerminationReason::OutOfFuel => {
        ExecError::Cancelled
      }
 
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
 
    VmError::PrototypeChainTooDeep => ExecError::Js(JsError {
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
 
fn describe_thrown_value(runtime: &mut vm_js::JsRuntime, value: Value) -> (Option<String>, String) {
  // Root the thrown value while we allocate property keys so GC cannot collect
  // it out from under us.
  let mut scope = runtime.heap.scope();
  let _ = scope.push_root(value);

  match value {
    Value::Object(obj) => {
      let typ = get_object_string_data_property(&mut scope, obj, "name");
      let message = get_object_string_data_property(&mut scope, obj, "message")
        .or_else(|| typ.clone())
        .unwrap_or_else(|| "<object>".to_string());
      (typ, message)
    }
 
    Value::Undefined => (None, "undefined".to_string()),
    Value::Null => (None, "null".to_string()),
    Value::Bool(b) => (None, b.to_string()),
    Value::Number(n) => (None, format_js_number(n)),
    Value::BigInt(b) => (None, b.to_decimal_string()),
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
        .and_then(|desc| scope.heap().get_string(desc).ok().map(|s| s.to_utf8_lossy()))
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
  use crate::report::ExpectedOutcome;
  use crate::harness::{assemble_source, HarnessMode};
  use std::path::PathBuf;
  use std::fs;
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
    let err = exec.execute(&test_case("cancel.js"), "1;", &cancel).unwrap_err();
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
}
