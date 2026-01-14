//! Test harness for comparing native execution against the `vm-js` oracle.
//!
//! The main responsibilities of this crate are:
//! - TS→JS erasure (TypeScript-only syntax removed/lowered into runnable JavaScript), and
//! - running the erased JavaScript under `vm-js` as a deterministic oracle.
//!
//! ## Promise-aware execution (`run_fixture*`)
//!
//! [`run_fixture`] / [`run_fixture_with_options`] execute `*.ts` and `*.js` fixture files and
//! expect the **script completion value** to be either:
//! - a JavaScript string, or
//! - a `Promise<string>`.
//!
//! When a fixture returns a Promise, the harness performs microtask checkpoints (draining the VM's
//! microtask queue) until the Promise settles, with explicit caps to ensure the harness never
//! hangs.
//!
//! ## Global observation protocol (`run_fixture_ts*`)
//!
//! For deterministic value-comparison fixtures, [`run_fixture_ts`] runs TypeScript source and
//! returns `String(globalThis.__native_result)` after a microtask checkpoint.

pub mod expectations;
pub mod fixtures;
#[cfg(feature = "native-js-runner")]
pub mod native_js_runner;

use std::fs;
use std::path::{Path, PathBuf};
use std::{collections::HashMap, mem};

use diagnostics::{Diagnostic, FileId, Span, TextRange};
use emit_js::{emit_top_level_diagnostic, EmitOptions};
use parse_js::{parse_with_options, Dialect, ParseOptions, SourceType};
use vm_js::{
  finish_loading_imported_module, format_stack_trace, format_termination, load_requested_modules,
  GcObject, Heap, HeapLimits, HostDefined, Job, JsRuntime, MicrotaskQueue, ModuleGraph, ModuleId,
  ModuleLoadPayload, ModuleReferrer, ModuleRequest, PromiseState, RealmId, RootId, Scope,
  SourceText, SourceTextInput, SourceTextModuleRecord, Value, Vm, VmError, VmHost, VmHostHooks,
  VmJobContext, VmOptions,
};

const OBSERVE_SCRIPT: &str = "String(globalThis.__native_result)";
const OBSERVE_SOURCE_NAME: &str = "<native-oracle-observe>";

const NATIVE_PRINT_NAME: &str = "__native_print";
const NATIVE_EPRINT_NAME: &str = "__native_eprint";
const NATIVE_BUILTINS_PRELUDE_SOURCE_NAME: &str = "<native-oracle-builtins>";
const NATIVE_BUILTINS_PRELUDE_SCRIPT: &str = r#"
globalThis.print = (...values) => __native_print(...values);
globalThis.console = { log: (...values) => __native_print(...values), error: (...values) => __native_eprint(...values) };
globalThis.assert = (cond, msg) => {
  if (!cond) throw new Error(msg === undefined ? "assertion failed" : String(msg));
};
globalThis.panic = (msg) => {
  throw new Error(msg === undefined ? "panic" : String(msg));
};
globalThis.trap = () => {
  throw new Error("trap");
};
"#;

struct RootOnlyJobCtx<'a> {
  heap: &'a mut Heap,
}

impl VmJobContext for RootOnlyJobCtx<'_> {
  fn call(
    &mut self,
    _hooks: &mut dyn VmHostHooks,
    _callee: Value,
    _this: Value,
    _args: &[Value],
  ) -> Result<Value, VmError> {
    Err(VmError::Unimplemented("RootOnlyJobCtx::call"))
  }

  fn construct(
    &mut self,
    _hooks: &mut dyn VmHostHooks,
    _callee: Value,
    _args: &[Value],
    _new_target: Value,
  ) -> Result<Value, VmError> {
    Err(VmError::Unimplemented("RootOnlyJobCtx::construct"))
  }

  fn add_root(&mut self, value: Value) -> Result<RootId, VmError> {
    self.heap.add_root(value)
  }

  fn remove_root(&mut self, id: RootId) {
    self.heap.remove_root(id);
  }
}

fn teardown_microtask_queue(heap: &mut Heap, queue: &mut MicrotaskQueue) {
  if queue.is_empty() {
    return;
  }

  let mut ctx = RootOnlyJobCtx { heap };
  queue.teardown(&mut ctx);
}

fn teardown_microtasks(rt: &mut JsRuntime) {
  if rt.vm.microtask_queue().is_empty() {
    return;
  }
  let (vm, heap) = (&mut rt.vm, &mut rt.heap);
  let mut ctx = RootOnlyJobCtx { heap };
  vm.microtask_queue_mut().teardown(&mut ctx);
}
#[derive(Debug)]
pub enum TsToJsError {
  Parse(parse_js::error::SyntaxError),
  Erase(Vec<Diagnostic>),
  Emit(Diagnostic),
  #[cfg(feature = "optimize-js-fallback")]
  Optimize(Vec<optimize_js::Diagnostic>),
  #[cfg(feature = "optimize-js-fallback")]
  OptimizeEmit(optimize_js::ProgramToJsError),
}

impl std::fmt::Display for TsToJsError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      TsToJsError::Parse(err) => write!(f, "{err}"),
      TsToJsError::Erase(diagnostics) => {
        write!(
          f,
          "ts-erase TS→JS erasure failed with {} diagnostic(s)",
          diagnostics.len()
        )?;
        if let Some(first) = diagnostics.first() {
          write!(f, ": {}: {}", first.code, first.message)?;
        }
        Ok(())
      }
      TsToJsError::Emit(diag) => {
        write!(
          f,
          "emit-js TS→JS emission failed: {}: {}",
          diag.code, diag.message
        )
      }
      #[cfg(feature = "optimize-js-fallback")]
      TsToJsError::Optimize(diagnostics) => {
        write!(
          f,
          "optimize-js TS→JS fallback failed with {} diagnostic(s)",
          diagnostics.len()
        )?;
        if let Some(first) = diagnostics.first() {
          write!(f, ": {}: {}", first.code, first.message)?;
        }
        Ok(())
      }
      #[cfg(feature = "optimize-js-fallback")]
      TsToJsError::OptimizeEmit(err) => {
        write!(f, "optimize-js TS→JS fallback emit failed: {err:?}")
      }
    }
  }
}

impl std::error::Error for TsToJsError {}

/// Erase TypeScript-only syntax from `source`, returning JavaScript that can be executed by the
/// oracle VM.
///
/// This is intentionally a best-effort API:
/// - It first attempts to parse TS/TSX, erase it via `ts-erase` (`StrictNative` mode), and emit JS
///   via `emit-js`.
/// - If emission fails and the `optimize-js-fallback` feature is enabled, it falls back to
///   `optimize-js`'s decompiler, which supports a wider range of syntax (but is significantly
///   heavier).
pub fn erase_typescript_to_js_with_source_type(
  source: &str,
  source_type: SourceType,
) -> Result<String, TsToJsError> {
  let file = FileId(0);

  // Prefer TS parsing; fall back to TSX for JSX-heavy inputs.
  let mut last_error = None;
  let mut parsed = None;
  for dialect in [Dialect::Ts, Dialect::Tsx] {
    let opts = ParseOptions {
      dialect,
      source_type,
    };
    match parse_with_options(source, opts) {
      Ok(ast) => {
        parsed = Some(ast);
        break;
      }
      Err(err) => last_error = Some(err),
    }
  }

  let mut top_level = match parsed {
    Some(ast) => ast,
    None => return Err(TsToJsError::Parse(last_error.expect("parse attempted"))),
  };

  if let Err(diags) = ts_erase::erase_types_strict_native(file, source_type, &mut top_level) {
    return erase_with_optimize_js_fallback(source, source_type, TsToJsError::Erase(diags));
  }

  match emit_top_level_diagnostic(file, &top_level, EmitOptions::minified()) {
    Ok(output) => Ok(output),
    Err(diag) => erase_with_optimize_js_fallback(source, source_type, TsToJsError::Emit(diag)),
  }
}

/// Backward-compatible wrapper for erasing TypeScript parsed as a classic (non-module) script.
pub fn erase_typescript_to_js(source: &str) -> Result<String, TsToJsError> {
  erase_typescript_to_js_with_source_type(source, SourceType::Script)
}

/// Typecheck TypeScript source in strict-native mode.
///
/// This is a compile-time acceptance gate: if any [`diagnostics::Severity::Error`] diagnostics are
/// emitted, the check fails and the full diagnostic list is returned.
///
/// This API is feature-gated because the `typecheck-ts` crate is intentionally not pulled into the
/// default `native-oracle-harness` build.
#[cfg(feature = "typecheck-strict-native")]
pub fn typecheck_strict_native(name: &str, source: &str) -> Result<(), Vec<Diagnostic>> {
  use typecheck_ts::lib_support::{CompilerOptions, ScriptTarget};
  use typecheck_ts::{FileKey, MemoryHost, Program};

  let mut options = CompilerOptions::default();
  // Prefer the new option name, but keep the legacy alias set for compatibility.
  options.native_strict = true;
  options.strict_native = true;
  options.target = ScriptTarget::Es2020;

  let mut host = MemoryHost::with_options(options);
  let file = FileKey::new(name);
  host.insert(file.clone(), source);

  let program = Program::new(host, vec![file]);
  let diagnostics = program.check();
  if diagnostics
    .iter()
    .any(|diag| diag.severity == diagnostics::Severity::Error)
  {
    Err(diagnostics)
  } else {
    Ok(())
  }
}

/// Like [`erase_typescript_to_js`], but only after the input passes strict-native typechecking.
///
/// This is intended for future `native-vs-oracle` suites that want to ensure the oracle is only
/// run on programs accepted by the strict-native dialect.
#[cfg(feature = "typecheck-strict-native")]
pub fn erase_typescript_to_js_checked_strict_native(
  name: &str,
  source: &str,
) -> Result<String, Diagnostic> {
  typecheck_strict_native(name, source).map_err(diagnostics_to_one)?;
  ts_to_js(source)
}

#[cfg(feature = "optimize-js-fallback")]
fn erase_with_optimize_js_fallback(
  source: &str,
  source_type: SourceType,
  _original: TsToJsError,
) -> Result<String, TsToJsError> {
  use optimize_js::{compile_source, program_to_js, DecompileOptions, TopLevelMode};

  let mode = match source_type {
    SourceType::Script => TopLevelMode::Script,
    SourceType::Module => TopLevelMode::Module,
  };
  let program = compile_source(source, mode, false).map_err(TsToJsError::Optimize)?;
  let bytes = program_to_js(
    &program,
    &DecompileOptions::default(),
    EmitOptions::minified(),
  )
  .map_err(TsToJsError::OptimizeEmit)?;
  Ok(String::from_utf8(bytes).expect("optimize-js emits UTF-8"))
}

#[cfg(not(feature = "optimize-js-fallback"))]
fn erase_with_optimize_js_fallback(
  _source: &str,
  _source_type: SourceType,
  original: TsToJsError,
) -> Result<String, TsToJsError> {
  Err(original)
}

#[derive(Debug, Clone)]
pub struct OracleHarnessOptions {
  /// Maximum number of microtask checkpoints to perform while waiting for a returned Promise to
  /// settle.
  pub max_microtask_checkpoints: usize,
  /// VM construction options (budgets, interrupt flags, etc).
  pub vm_options: VmOptions,
  /// Heap memory limits.
  pub heap_limits: HeapLimits,
}

impl Default for OracleHarnessOptions {
  fn default() -> Self {
    Self {
      // Most Promises should settle in a single checkpoint, but keep a generous cap to guard
      // against pathological promise chains.
      max_microtask_checkpoints: 128,
      vm_options: VmOptions {
        // Deterministic safety: fuel bounds execution without relying on wall-clock timeouts.
        default_fuel: Some(200_000),
        ..VmOptions::default()
      },
      // Keep the harness conservative: fixtures should be small and deterministic.
      heap_limits: HeapLimits::new(32 * 1024 * 1024, 8 * 1024 * 1024),
    }
  }
}

#[derive(Debug, thiserror::Error)]
pub enum OracleHarnessError {
  #[error(transparent)]
  Io(#[from] std::io::Error),

  #[error(transparent)]
  TsToJs(#[from] TsToJsError),

  #[error("fixture returned non-string value: {value:?}")]
  NonStringReturn { value: Value },

  #[error("promise fulfilled with non-string value: {value:?}")]
  NonStringPromiseFulfillment { value: Value },

  #[error("promise rejected: {reason}")]
  PromiseRejected { reason: String },

  #[error("promise did not settle after {microtask_checkpoints} microtask checkpoints")]
  PromiseDidNotSettle { microtask_checkpoints: usize },

  #[error("uncaught exception: {message}")]
  UncaughtException { message: String },

  #[error("execution terminated: {message}")]
  Terminated { message: String },

  #[error("vm error: {message}")]
  Vm { message: String },
}

#[derive(Default)]
struct ConsoleCapture {
  stdout: String,
  stderr: String,
}

fn format_console_line(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  args: &[Value],
) -> Result<String, VmError> {
  // `Scope::to_string` requires `&mut Vm`, so we must not hold a mutable borrow of
  // `vm.user_data_mut()` across the conversion loop.
  let mut line = String::new();
  for (idx, value) in args.iter().copied().enumerate() {
    if idx > 0 {
      line.push(' ');
    }

    let s = scope.to_string(vm, host, hooks, value)?;
    let js = scope.heap().get_string(s)?;
    line.push_str(&js.to_utf8_lossy());
  }
  line.push('\n');
  Ok(line)
}

fn native_print(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let line = format_console_line(vm, scope, host, hooks, args)?;
  let Some(capture) = vm.user_data_mut::<ConsoleCapture>() else {
    return Err(VmError::Unimplemented("console capture buffer missing"));
  };
  capture.stdout.push_str(&line);
  Ok(Value::Undefined)
}

fn native_eprint(
  vm: &mut Vm,
  scope: &mut Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let line = format_console_line(vm, scope, host, hooks, args)?;
  let Some(capture) = vm.user_data_mut::<ConsoleCapture>() else {
    return Err(VmError::Unimplemented("console capture buffer missing"));
  };
  capture.stderr.push_str(&line);
  Ok(Value::Undefined)
}

fn take_captured_console(vm: &mut Vm) -> (String, String) {
  let Some(capture) = vm.take_user_data::<ConsoleCapture>() else {
    return (String::new(), String::new());
  };
  let mut stdout = capture.stdout;
  let mut stderr = capture.stderr;
  if stdout.ends_with('\n') {
    stdout.pop();
  }
  if stderr.ends_with('\n') {
    stderr.pop();
  }
  (stdout, stderr)
}

fn install_native_builtins(rt: &mut JsRuntime) -> Result<(), VmError> {
  rt.register_global_native_function(NATIVE_PRINT_NAME, native_print, 0)?;
  rt.register_global_native_function(NATIVE_EPRINT_NAME, native_eprint, 0)?;
  let source = SourceText::new_charged_arc(
    &mut rt.heap,
    NATIVE_BUILTINS_PRELUDE_SOURCE_NAME,
    NATIVE_BUILTINS_PRELUDE_SCRIPT,
  )?;
  rt.exec_script_source(source)?;
  Ok(())
}

/// Structured result of executing a snippet under either the oracle VM or a native backend.
///
/// This is intended for oracle-vs-native comparisons: it distinguishes between successful
/// completion, uncaught exceptions, termination (fuel/timeout/OOM/etc), and compilation errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
  Ok {
    value: String,
    stdout: String,
    stderr: String,
  },
  Throw {
    message: String,
    stack: Option<String>,
    stdout: String,
    stderr: String,
  },
  Terminated {
    message: String,
    stdout: String,
    stderr: String,
  },
  CompileError {
    diagnostic: Diagnostic,
  },
}

fn attach_stdio(outcome: RunOutcome, stdout: String, stderr: String) -> RunOutcome {
  match outcome {
    RunOutcome::Ok { value, .. } => RunOutcome::Ok {
      value,
      stdout,
      stderr,
    },
    RunOutcome::Throw { message, stack, .. } => RunOutcome::Throw {
      message,
      stack,
      stdout,
      stderr,
    },
    RunOutcome::Terminated { message, .. } => RunOutcome::Terminated {
      message,
      stdout,
      stderr,
    },
    other @ RunOutcome::CompileError { .. } => other,
  }
}

#[derive(Debug, Clone, Copy)]
pub struct RunOutcomeCompareOptions {
  pub compare_stdout: bool,
  pub compare_stderr: bool,
  pub compare_stack: bool,
}

impl Default for RunOutcomeCompareOptions {
  fn default() -> Self {
    Self {
      // Most current deterministic fixtures only validate a stable value, but keep the comparison
      // extensible: suites can opt into stdio and stack comparisons when available.
      compare_stdout: false,
      compare_stderr: false,
      compare_stack: false,
    }
  }
}

/// Compare two [`RunOutcome`]s for oracle-vs-native equivalence.
///
/// Returns `Ok(())` when the outcomes match under `options`, or an explanatory message when they
/// differ.
pub fn compare_run_outcomes(
  expected: &RunOutcome,
  actual: &RunOutcome,
  options: RunOutcomeCompareOptions,
) -> Result<(), String> {
  match (expected, actual) {
    (
      RunOutcome::Ok {
        value: ev,
        stdout: eout,
        stderr: eerr,
      },
      RunOutcome::Ok {
        value: av,
        stdout: aout,
        stderr: aerr,
      },
    ) => {
      if ev != av {
        return Err(format!("Ok.value mismatch: expected {ev:?}, got {av:?}"));
      }
      if options.compare_stdout && eout != aout {
        return Err(format!(
          "Ok.stdout mismatch: expected {eout:?}, got {aout:?}"
        ));
      }
      if options.compare_stderr && eerr != aerr {
        return Err(format!(
          "Ok.stderr mismatch: expected {eerr:?}, got {aerr:?}"
        ));
      }
      Ok(())
    }
    (
      RunOutcome::Throw {
        message: em,
        stack: estack,
        stdout: eout,
        stderr: eerr,
      },
      RunOutcome::Throw {
        message: am,
        stack: astack,
        stdout: aout,
        stderr: aerr,
      },
    ) => {
      if em != am {
        return Err(format!(
          "Throw.message mismatch: expected {em:?}, got {am:?}"
        ));
      }
      if options.compare_stack && estack != astack {
        return Err(format!(
          "Throw.stack mismatch: expected {estack:?}, got {astack:?}"
        ));
      }
      if options.compare_stdout && eout != aout {
        return Err(format!(
          "Throw.stdout mismatch: expected {eout:?}, got {aout:?}"
        ));
      }
      if options.compare_stderr && eerr != aerr {
        return Err(format!(
          "Throw.stderr mismatch: expected {eerr:?}, got {aerr:?}"
        ));
      }
      Ok(())
    }
    (
      RunOutcome::Terminated {
        message: em,
        stdout: eout,
        stderr: eerr,
      },
      RunOutcome::Terminated {
        message: am,
        stdout: aout,
        stderr: aerr,
      },
    ) => {
      if em != am {
        return Err(format!(
          "Terminated.message mismatch: expected {em:?}, got {am:?}"
        ));
      }
      if options.compare_stdout && eout != aout {
        return Err(format!(
          "Terminated.stdout mismatch: expected {eout:?}, got {aout:?}"
        ));
      }
      if options.compare_stderr && eerr != aerr {
        return Err(format!(
          "Terminated.stderr mismatch: expected {eerr:?}, got {aerr:?}"
        ));
      }
      Ok(())
    }
    (RunOutcome::CompileError { diagnostic: ed }, RunOutcome::CompileError { diagnostic: ad }) => {
      // Spans can differ between compilers/backends; compare only the stable diagnostic identity.
      if ed.code != ad.code || ed.message != ad.message {
        return Err(format!(
          "CompileError mismatch: expected {}: {}, got {}: {}",
          ed.code, ed.message, ad.code, ad.message
        ));
      }
      Ok(())
    }
    _ => Err(format!(
      "outcome variant mismatch: expected {expected:?}, got {actual:?}"
    )),
  }
}

/// Runs `ts` in the `vm-js` oracle and `native`, then compares the two outcomes.
pub fn compare_native_against_vm_js_oracle(
  native: &impl NativeRunner2,
  ts: &str,
  options: RunOutcomeCompareOptions,
) -> Result<(), String> {
  let oracle = run_fixture_ts_outcome(ts);
  let native = NativeRunner2::compile_and_run(native, ts);
  compare_run_outcomes(&oracle, &native, options)
}

/// Execute a fixture file (TypeScript or JavaScript) and return its output string.
pub fn run_fixture(path: impl AsRef<Path>) -> Result<String, OracleHarnessError> {
  run_fixture_with_options(path, &OracleHarnessOptions::default())
}

pub fn run_fixture_with_options(
  path: impl AsRef<Path>,
  options: &OracleHarnessOptions,
) -> Result<String, OracleHarnessError> {
  let path = path.as_ref();
  let source_name = path
    .file_name()
    .and_then(|s| s.to_str())
    .unwrap_or("<fixture>");
  let source_text = fs::read_to_string(path)?;
  let ext = path.extension().and_then(|ext| ext.to_str());
  if matches!(ext, Some("ts") | Some("tsx")) {
    run_typescript_source_with_options(source_name, &source_text, options)
  } else {
    // JS fixtures bypass the TS→JS pipeline; this keeps the promise/microtask
    // corpus independent of `emit-js` feature coverage.
    run_js_source_with_options(source_name, source_text, options)
  }
}

/// Execute a fixture file (TypeScript or JavaScript) and return a structured [`RunOutcome`].
///
/// This mirrors [`run_fixture`] / [`run_fixture_with_options`], but:
/// - returns structured outcomes (ok/throw/termination/compile error), and
/// - captures `console.log` output into [`RunOutcome::stdout`].
pub fn run_fixture_outcome(path: impl AsRef<Path>) -> RunOutcome {
  run_fixture_outcome_with_options(path, &OracleHarnessOptions::default())
}

pub fn run_fixture_outcome_with_options(
  path: impl AsRef<Path>,
  options: &OracleHarnessOptions,
) -> RunOutcome {
  let path = path.as_ref();
  let source_name = path
    .file_name()
    .and_then(|s| s.to_str())
    .unwrap_or("<fixture>");
  let source_text = match fs::read_to_string(path) {
    Ok(src) => src,
    Err(err) => {
      return RunOutcome::Terminated {
        message: format!("failed to read fixture {}: {err}", path.display()),
        stdout: String::new(),
        stderr: String::new(),
      }
    }
  };
  let ext = path.extension().and_then(|ext| ext.to_str());
  if matches!(ext, Some("ts") | Some("tsx")) {
    run_typescript_source_outcome_with_options(source_name, &source_text, options)
  } else {
    run_js_source_outcome_with_options(source_name, source_text, options)
  }
}

/// Execute a TypeScript snippet in the oracle VM, returning a structured [`RunOutcome`].
pub fn run_typescript_source_outcome_with_options<'a>(
  source_name: impl Into<SourceTextInput<'a>>,
  source_text: &str,
  options: &OracleHarnessOptions,
) -> RunOutcome {
  let js = match ts_to_js(source_text) {
    Ok(js) => js,
    Err(diag) => return RunOutcome::CompileError { diagnostic: diag },
  };
  run_js_source_outcome_with_options(source_name, js, options)
}

/// Execute already-erased JavaScript source, returning a structured [`RunOutcome`].
///
/// This captures output written via `print(...)`, `console.log(...)`, and `console.error(...)` into
/// [`RunOutcome::stdout`] / [`RunOutcome::stderr`] using the same native builtins prelude as
/// [`run_js_source_capture_stdout_with_options`].
pub fn run_js_source_outcome_with_options<'a>(
  source_name: impl Into<SourceTextInput<'a>>,
  source_text: impl Into<SourceTextInput<'a>>,
  options: &OracleHarnessOptions,
) -> RunOutcome {
  let mut rt = match new_runtime_with_options(options) {
    Ok(rt) => rt,
    Err(err) => {
      return RunOutcome::Terminated {
        message: format!("failed to init vm-js: {err}"),
        stdout: String::new(),
        stderr: String::new(),
      }
    }
  };
  rt.vm.set_user_data(ConsoleCapture::default());

  let outcome = (|| {
    if let Err(err) = install_native_builtins(&mut rt) {
      return vm_error_to_outcome(&mut rt, err);
    }

    let source = match SourceText::new_charged_arc(&mut rt.heap, source_name, source_text) {
      Ok(source) => source,
      Err(err) => return vm_error_to_outcome(&mut rt, err),
    };
    let value = match rt.exec_script_source(source) {
      Ok(v) => v,
      Err(err) => return vm_error_to_outcome(&mut rt, err),
    };

    match value_to_outcome_string(&mut rt, value, options) {
      Ok(value) => RunOutcome::Ok {
        value,
        stdout: String::new(),
        stderr: String::new(),
      },
      Err(outcome) => outcome,
    }
  })();

  let (stdout, stderr) = take_captured_console(&mut rt.vm);
  teardown_microtasks(&mut rt);
  attach_stdio(outcome, stdout, stderr)
}

/// Execute a TypeScript snippet in the oracle VM, returning its output string.
pub fn run_typescript_source_with_options<'a>(
  source_name: impl Into<SourceTextInput<'a>>,
  source_text: &str,
  options: &OracleHarnessOptions,
) -> Result<String, OracleHarnessError> {
  let js = erase_typescript_to_js(source_text)?;
  run_js_source_with_options(source_name, js, options)
}

/// Execute already-erased JavaScript source, returning its output string.
pub fn run_js_source_with_options<'a>(
  source_name: impl Into<SourceTextInput<'a>>,
  source_text: impl Into<SourceTextInput<'a>>,
  options: &OracleHarnessOptions,
) -> Result<String, OracleHarnessError> {
  let mut rt = new_runtime_with_options(options).map_err(|e| OracleHarnessError::Vm {
    message: e.to_string(),
  })?;
  let source =
    SourceText::new_charged_arc(&mut rt.heap, source_name, source_text).map_err(|err| {
      OracleHarnessError::Vm {
        message: err.to_string(),
      }
    })?;

  let result = rt.exec_script_source(source);
  let out = match result {
    Ok(value) => value_to_fixture_string(&mut rt, value, options),
    Err(err) => Err(map_vm_error(&mut rt, err)),
  };

  // `vm-js` jobs can hold persistent roots; never drop a runtime with queued jobs still pending.
  teardown_microtasks(&mut rt);
  out
}

/// Execute already-erased JavaScript source, capturing `print` / `console.log` output.
///
/// ## Native builtins contract
///
/// `vm-js` intentionally has no `print`/`console` builtins. For output-based oracle comparisons,
/// this harness injects a minimal native-js-style prelude before evaluating `source_text`:
///
/// - `globalThis.print = (...values) => __native_print(...values);`
/// - `globalThis.console = { log: (...values) => __native_print(...values), error: (...values) => __native_eprint(...values) };`
/// - `globalThis.assert = (cond, msg?) => { if (!cond) throw new Error(...); }`
/// - `globalThis.panic = (msg?) => { throw new Error(...); }`
/// - `globalThis.trap = () => { throw new Error('trap'); }`
///
/// Each `print(...)` / `console.log(...)` call appends
/// `String(a) + " " + String(b) + ... + "\n"` to an internal host-owned stdout buffer (arguments are
/// joined with a single ASCII space). Each `console.error(...)` call appends to a stderr buffer.
///
/// The returned string is the concatenated buffer with a single trailing `\n` removed (if present),
/// matching the common “captured stdout” convention used by this repository's fixture runners.
pub fn run_js_source_capture_stdout_with_options<'a>(
  source_name: impl Into<SourceTextInput<'a>>,
  source_text: impl Into<SourceTextInput<'a>>,
  options: &OracleHarnessOptions,
) -> Result<String, OracleHarnessError> {
  let mut rt = new_runtime_with_options(options).map_err(|e| OracleHarnessError::Vm {
    message: e.to_string(),
  })?;
  rt.vm.set_user_data(ConsoleCapture::default());

  let out = (|| {
    install_native_builtins(&mut rt).map_err(|err| map_vm_error(&mut rt, err))?;

    let source = SourceText::new_charged_arc(&mut rt.heap, source_name, source_text)
      .map_err(|err| map_vm_error(&mut rt, err))?;
    rt.exec_script_source(source)
      .map_err(|err| map_vm_error(&mut rt, err))?;

    rt.vm
      .perform_microtask_checkpoint(&mut rt.heap)
      .map_err(|err| map_vm_error(&mut rt, err))?;

    let capture =
      rt.vm
        .take_user_data::<ConsoleCapture>()
        .ok_or_else(|| OracleHarnessError::Vm {
          message: "stdout capture buffer missing".to_string(),
        })?;

    let mut out = capture.stdout;
    if out.ends_with('\n') {
      out.pop();
    }
    Ok(out)
  })();

  // `vm-js` jobs can hold persistent roots; never drop a runtime with queued jobs still pending.
  teardown_microtasks(&mut rt);
  out
}

/// Execute a TypeScript snippet in the oracle VM, capturing `console.log` output.
pub fn run_typescript_source_capture_stdout_with_options<'a>(
  source_name: impl Into<SourceTextInput<'a>>,
  source_text: &str,
  options: &OracleHarnessOptions,
) -> Result<String, OracleHarnessError> {
  let js = erase_typescript_to_js(source_text)?;
  run_js_source_capture_stdout_with_options(source_name, js, options)
}

/// Execute a fixture file (TypeScript or JavaScript) and return captured `console.log` output.
pub fn run_fixture_capture_stdout(path: impl AsRef<Path>) -> Result<String, OracleHarnessError> {
  run_fixture_capture_stdout_with_options(path, &OracleHarnessOptions::default())
}

pub fn run_fixture_capture_stdout_with_options(
  path: impl AsRef<Path>,
  options: &OracleHarnessOptions,
) -> Result<String, OracleHarnessError> {
  let path = path.as_ref();
  let source_name = path
    .file_name()
    .and_then(|s| s.to_str())
    .unwrap_or("<fixture>");
  let source_text = fs::read_to_string(path)?;
  let ext = path.extension().and_then(|ext| ext.to_str());
  if matches!(ext, Some("ts") | Some("tsx")) {
    run_typescript_source_capture_stdout_with_options(source_name, &source_text, options)
  } else {
    run_js_source_capture_stdout_with_options(source_name, source_text, options)
  }
}

/// Execute JavaScript source with a deterministic set of native-style builtins enabled, capturing
/// output written via `print(...)` / `console.log(...)`.
///
/// This API:
/// - installs builtins via a short prelude script,
/// - executes `source_text`,
/// - performs a microtask checkpoint (so `Promise.then(...)` prints are observable),
/// - and returns the captured stdout buffer.
pub fn run_js_source_with_native_builtins_capture_stdout<'a>(
  source_name: impl Into<SourceTextInput<'a>>,
  source_text: impl Into<SourceTextInput<'a>>,
) -> Result<String, OracleHarnessError> {
  run_js_source_with_native_builtins_capture_stdout_with_options(
    source_name,
    source_text,
    &OracleHarnessOptions::default(),
  )
}

pub fn run_js_source_with_native_builtins_capture_stdout_with_options<'a>(
  source_name: impl Into<SourceTextInput<'a>>,
  source_text: impl Into<SourceTextInput<'a>>,
  options: &OracleHarnessOptions,
) -> Result<String, OracleHarnessError> {
  let mut rt = new_runtime_with_options(options).map_err(|e| OracleHarnessError::Vm {
    message: e.to_string(),
  })?;
  rt.vm.set_user_data(ConsoleCapture::default());

  let out = (|| {
    rt.register_global_native_function(NATIVE_PRINT_NAME, native_print, 0)
      .map_err(|err| map_vm_error(&mut rt, err))?;
    rt.register_global_native_function(NATIVE_EPRINT_NAME, native_eprint, 0)
      .map_err(|err| map_vm_error(&mut rt, err))?;

    let prelude = SourceText::new_charged_arc(
      &mut rt.heap,
      NATIVE_BUILTINS_PRELUDE_SOURCE_NAME,
      NATIVE_BUILTINS_PRELUDE_SCRIPT,
    )
    .map_err(|err| map_vm_error(&mut rt, err))?;
    rt.exec_script_source(prelude)
      .map_err(|err| map_vm_error(&mut rt, err))?;

    let source = SourceText::new_charged_arc(&mut rt.heap, source_name, source_text)
      .map_err(|err| map_vm_error(&mut rt, err))?;
    rt.exec_script_source(source)
      .map_err(|err| map_vm_error(&mut rt, err))?;

    rt.vm
      .perform_microtask_checkpoint(&mut rt.heap)
      .map_err(|err| map_vm_error(&mut rt, err))?;

    let capture =
      rt.vm
        .take_user_data::<ConsoleCapture>()
        .ok_or_else(|| OracleHarnessError::Vm {
          message: "stdout capture buffer missing".to_string(),
        })?;
    Ok(capture.stdout)
  })();

  teardown_microtasks(&mut rt);
  out
}

/// Execute a TypeScript snippet in the oracle VM with native builtins enabled, returning captured
/// stdout.
pub fn run_typescript_source_with_native_builtins_capture_stdout<'a>(
  source_name: impl Into<SourceTextInput<'a>>,
  source_text: &str,
) -> Result<String, OracleHarnessError> {
  run_typescript_source_with_native_builtins_capture_stdout_with_options(
    source_name,
    source_text,
    &OracleHarnessOptions::default(),
  )
}

pub fn run_typescript_source_with_native_builtins_capture_stdout_with_options<'a>(
  source_name: impl Into<SourceTextInput<'a>>,
  source_text: &str,
  options: &OracleHarnessOptions,
) -> Result<String, OracleHarnessError> {
  let js = erase_typescript_to_js(source_text)?;
  run_js_source_with_native_builtins_capture_stdout_with_options(source_name, js, options)
}

fn value_to_fixture_string(
  rt: &mut JsRuntime,
  value: Value,
  options: &OracleHarnessOptions,
) -> Result<String, OracleHarnessError> {
  // Fast path: synchronous string.
  if let Value::String(s) = value {
    return Ok(
      rt.heap()
        .get_string(s)
        .map(|js| js.to_utf8_lossy())
        .map_err(|e| OracleHarnessError::Vm {
          message: e.to_string(),
        })?,
    );
  }

  // Await Promise<string>.
  let Value::Object(obj) = value else {
    return Err(OracleHarnessError::NonStringReturn { value });
  };
  if !rt.heap().is_promise(obj) {
    return Err(OracleHarnessError::NonStringReturn { value });
  }

  // Root the promise so microtask execution can allocate/GC without invalidating the handle.
  let root_id = rt
    .heap_mut()
    .add_root(Value::Object(obj))
    .map_err(|e| OracleHarnessError::Vm {
      message: e.to_string(),
    })?;

  let settle_result = (|| wait_for_promise(rt, obj, options))();
  rt.heap_mut().remove_root(root_id);
  settle_result
}

fn wait_for_promise(
  rt: &mut JsRuntime,
  promise: vm_js::GcObject,
  options: &OracleHarnessOptions,
) -> Result<String, OracleHarnessError> {
  let mut checkpoints = 0usize;

  loop {
    let state = rt
      .heap()
      .promise_state(promise)
      .map_err(|e| OracleHarnessError::Vm {
        message: e.to_string(),
      })?;

    match state {
      PromiseState::Pending => {
        if checkpoints >= options.max_microtask_checkpoints {
          return Err(OracleHarnessError::PromiseDidNotSettle {
            microtask_checkpoints: checkpoints,
          });
        }

        // If there is nothing queued, the promise cannot make progress (this harness does not have
        // a macro-task/event loop).
        if rt.vm.microtask_queue().is_empty() {
          return Err(OracleHarnessError::PromiseDidNotSettle {
            microtask_checkpoints: checkpoints,
          });
        }

        rt.vm
          .perform_microtask_checkpoint(&mut rt.heap)
          .map_err(|e| map_vm_error(rt, e))?;
        checkpoints += 1;
      }
      PromiseState::Fulfilled => {
        let value = rt
          .heap()
          .promise_result(promise)
          .map_err(|e| OracleHarnessError::Vm {
            message: e.to_string(),
          })?
          .unwrap_or(Value::Undefined);
        let Value::String(s) = value else {
          return Err(OracleHarnessError::NonStringPromiseFulfillment { value });
        };
        return Ok(
          rt.heap()
            .get_string(s)
            .map(|js| js.to_utf8_lossy())
            .map_err(|e| OracleHarnessError::Vm {
              message: e.to_string(),
            })?,
        );
      }
      PromiseState::Rejected => {
        let reason = rt
          .heap()
          .promise_result(promise)
          .map_err(|e| OracleHarnessError::Vm {
            message: e.to_string(),
          })?
          .unwrap_or(Value::Undefined);
        return Err(OracleHarnessError::PromiseRejected {
          reason: stringify_value(rt, reason, 0),
        });
      }
    }
  }
}

fn value_to_outcome_string(
  rt: &mut JsRuntime,
  value: Value,
  options: &OracleHarnessOptions,
) -> Result<String, RunOutcome> {
  // Fast path: synchronous string.
  if let Value::String(s) = value {
    return rt
      .heap()
      .get_string(s)
      .map(|js| js.to_utf8_lossy())
      .map_err(|err| RunOutcome::Terminated {
        message: format!("invalid string handle: {err}"),
        stdout: String::new(),
        stderr: String::new(),
      });
  }

  // Await Promise<string>.
  let Value::Object(obj) = value else {
    return Err(RunOutcome::Terminated {
      message: format!("fixture returned non-string value: {value:?}"),
      stdout: String::new(),
      stderr: String::new(),
    });
  };
  if !rt.heap().is_promise(obj) {
    return Err(RunOutcome::Terminated {
      message: format!("fixture returned non-string value: {value:?}"),
      stdout: String::new(),
      stderr: String::new(),
    });
  }

  // Root the promise so microtask execution can allocate/GC without invalidating the handle.
  let root_id = match rt.heap_mut().add_root(Value::Object(obj)) {
    Ok(id) => id,
    Err(err) => return Err(vm_error_to_outcome(rt, err)),
  };

  let settle_result = (|| wait_for_promise_outcome(rt, obj, options))();
  rt.heap_mut().remove_root(root_id);
  settle_result
}

fn wait_for_promise_outcome(
  rt: &mut JsRuntime,
  promise: vm_js::GcObject,
  options: &OracleHarnessOptions,
) -> Result<String, RunOutcome> {
  let mut checkpoints = 0usize;

  loop {
    let state = match rt.heap().promise_state(promise) {
      Ok(state) => state,
      Err(err) => return Err(vm_error_to_outcome(rt, err)),
    };

    match state {
      PromiseState::Pending => {
        if checkpoints >= options.max_microtask_checkpoints {
          return Err(RunOutcome::Terminated {
            message: format!(
              "promise did not settle after {microtask_checkpoints} microtask checkpoints",
              microtask_checkpoints = checkpoints
            ),
            stdout: String::new(),
            stderr: String::new(),
          });
        }

        // If there is nothing queued, the promise cannot make progress (this harness does not have
        // a macro-task/event loop).
        if rt.vm.microtask_queue().is_empty() {
          return Err(RunOutcome::Terminated {
            message: format!(
              "promise did not settle after {microtask_checkpoints} microtask checkpoints",
              microtask_checkpoints = checkpoints
            ),
            stdout: String::new(),
            stderr: String::new(),
          });
        }

        if let Err(err) = rt.vm.perform_microtask_checkpoint(&mut rt.heap) {
          return Err(vm_error_to_outcome(rt, err));
        }
        checkpoints += 1;
      }
      PromiseState::Fulfilled => {
        let value = match rt.heap().promise_result(promise) {
          Ok(v) => v.unwrap_or(Value::Undefined),
          Err(err) => return Err(vm_error_to_outcome(rt, err)),
        };
        let Value::String(s) = value else {
          return Err(RunOutcome::Terminated {
            message: format!("promise fulfilled with non-string value: {value:?}"),
            stdout: String::new(),
            stderr: String::new(),
          });
        };
        return rt
          .heap()
          .get_string(s)
          .map(|js| js.to_utf8_lossy())
          .map_err(|err| RunOutcome::Terminated {
            message: format!("invalid string handle: {err}"),
            stdout: String::new(),
            stderr: String::new(),
          });
      }
      PromiseState::Rejected => {
        let reason = match rt.heap().promise_result(promise) {
          Ok(v) => v.unwrap_or(Value::Undefined),
          Err(err) => return Err(vm_error_to_outcome(rt, err)),
        };
        return Err(RunOutcome::Throw {
          message: stringify_value(rt, reason, 0),
          stack: None,
          stdout: String::new(),
          stderr: String::new(),
        });
      }
    }
  }
}

fn map_vm_error(rt: &mut JsRuntime, err: VmError) -> OracleHarnessError {
  match err {
    VmError::Throw(value) => OracleHarnessError::UncaughtException {
      message: stringify_value(rt, value, 0),
    },
    VmError::ThrowWithStack { value, stack } => {
      // Keep error formatting stable and test-friendly: stringify the thrown value and append a
      // simple stack trace if present.
      let mut msg = stringify_value(rt, value, 0);
      if !stack.is_empty() {
        msg.push('\n');
        msg.push_str(&format_stack_trace(&stack));
      }
      OracleHarnessError::UncaughtException { message: msg }
    }
    VmError::Termination(term) => OracleHarnessError::Terminated {
      message: format_termination(&term),
    },
    other => OracleHarnessError::Vm {
      message: other.to_string(),
    },
  }
}

fn stringify_value(rt: &mut JsRuntime, value: Value, depth: usize) -> String {
  const MAX_DEPTH: usize = 8;
  if depth >= MAX_DEPTH {
    return format!("{value:?}");
  }

  // Avoid borrowing the heap across recursion: stringify the value in a nested scope, then drop it
  // before potentially recursing on thrown values.
  let mut host = ();
  let mut hooks = MicrotaskQueue::new();
  let res = {
    let mut scope = rt.heap.scope();
    scope.to_string(&mut rt.vm, &mut host, &mut hooks, value)
  };

  // String conversion can invoke user code and queue Promise jobs. We never execute microtasks
  // while formatting error messages, but we must still discard queued jobs to clean up any
  // persistent roots.
  teardown_microtask_queue(&mut rt.heap, &mut hooks);

  match res {
    Ok(s) => rt
      .heap()
      .get_string(s)
      .map(|js| js.to_utf8_lossy())
      .unwrap_or_else(|_| "<invalid string>".to_string()),
    Err(VmError::Throw(v) | VmError::ThrowWithStack { value: v, .. }) => {
      stringify_value(rt, v, depth + 1)
    }
    Err(_) => format!("{value:?}"),
  }
}

fn empty_span() -> Span {
  Span::new(FileId(0), TextRange::new(0, 0))
}

fn harness_error(message: impl Into<String>) -> Diagnostic {
  Diagnostic::error("ORACLE0001", message, empty_span())
}

fn diagnostics_to_one(mut diags: Vec<Diagnostic>) -> Diagnostic {
  if diags.is_empty() {
    return harness_error("operation returned no diagnostics");
  }
  let mut first = diags.remove(0);
  if !diags.is_empty() {
    first.push_note(format!("and {} more diagnostics", diags.len()));
    for diag in diags {
      first.push_note(format!("{}: {}", diag.code, diag.message));
    }
  }
  first
}

fn ts_to_js(ts: &str) -> Result<String, Diagnostic> {
  let file = FileId(0);
  erase_typescript_to_js_with_source_type(ts, SourceType::Script).map_err(|err| match err {
    TsToJsError::Parse(err) => err.to_diagnostic(file),
    TsToJsError::Erase(diags) => diagnostics_to_one(diags),
    TsToJsError::Emit(diag) => diag,
    #[cfg(feature = "optimize-js-fallback")]
    TsToJsError::Optimize(diags) => diagnostics_to_one(diags),
    #[cfg(feature = "optimize-js-fallback")]
    TsToJsError::OptimizeEmit(err) => {
      harness_error(format!("optimize-js TS→JS fallback failed: {err:?}"))
    }
  })
}

fn ts_to_js_with_source_type(ts: &str, source_type: SourceType) -> Result<String, Diagnostic> {
  let file = FileId(0);
  erase_typescript_to_js_with_source_type(ts, source_type).map_err(|err| match err {
    TsToJsError::Parse(err) => err.to_diagnostic(file),
    TsToJsError::Erase(diags) => diagnostics_to_one(diags),
    TsToJsError::Emit(diag) => diag,
    #[cfg(feature = "optimize-js-fallback")]
    TsToJsError::Optimize(diags) => diagnostics_to_one(diags),
    #[cfg(feature = "optimize-js-fallback")]
    TsToJsError::OptimizeEmit(err) => {
      harness_error(format!("optimize-js TS→JS fallback failed: {err:?}"))
    }
  })
}

fn new_runtime_with_options(options: &OracleHarnessOptions) -> Result<JsRuntime, VmError> {
  let vm = Vm::new(options.vm_options.clone());
  let heap = Heap::new(options.heap_limits);
  let mut rt = JsRuntime::new(vm, heap)?;
  // Reset the budget after runtime initialization so options apply deterministically to the guest
  // script (not to realm/builtin setup).
  rt.vm.reset_budget_to_default();
  Ok(rt)
}

fn value_to_string_in_heap(heap: &Heap, value: Value) -> String {
  match value {
    Value::Undefined => "undefined".to_string(),
    Value::Null => "null".to_string(),
    Value::Bool(b) => b.to_string(),
    Value::Number(n) => n.to_string(),
    Value::BigInt(b) => heap
      .get_bigint(b)
      .ok()
      .and_then(|bi| bi.to_string_radix_with_tick(10, &mut || Ok(())).ok())
      .unwrap_or_else(|| "<invalid bigint>".to_string()),
    Value::String(s) => heap
      .get_string(s)
      .map(|s| s.to_utf8_lossy())
      .unwrap_or_else(|_| "<invalid string>".to_string()),
    Value::Symbol(sym) => format!("<symbol {sym:?}>"),
    Value::Object(obj) => format!("<object {obj:?}>"),
  }
}

fn vm_error_to_diagnostic(rt: &JsRuntime, err: VmError) -> Diagnostic {
  vm_error_to_diagnostic_with_heap(rt.heap(), err)
}

fn vm_error_to_diagnostic_with_heap(heap: &Heap, err: VmError) -> Diagnostic {
  match err {
    VmError::Syntax(diags) => diagnostics_to_one(diags),
    VmError::ThrowWithStack { value, stack } => {
      let mut diag = harness_error(format!(
        "uncaught exception: {}",
        value_to_string_in_heap(heap, value)
      ));
      if !stack.is_empty() {
        diag.push_note(format_stack_trace(&stack));
      }
      diag
    }
    VmError::Throw(value) => harness_error(format!(
      "uncaught exception: {}",
      value_to_string_in_heap(heap, value)
    )),
    VmError::Termination(term) => harness_error(format_termination(&term)),
    other => harness_error(other.to_string()),
  }
}

fn vm_error_to_outcome(rt: &mut JsRuntime, err: VmError) -> RunOutcome {
  let stdout = String::new();
  let stderr = String::new();
  match err {
    VmError::Syntax(diags) => RunOutcome::CompileError {
      diagnostic: diagnostics_to_one(diags),
    },
    VmError::Throw(value) => RunOutcome::Throw {
      message: stringify_value(rt, value, 0),
      stack: None,
      stdout,
      stderr,
    },
    VmError::ThrowWithStack { value, stack } => RunOutcome::Throw {
      message: stringify_value(rt, value, 0),
      stack: (!stack.is_empty()).then(|| format_stack_trace(&stack)),
      stdout,
      stderr,
    },
    VmError::Termination(term) => RunOutcome::Terminated {
      message: format_termination(&term),
      stdout,
      stderr,
    },
    other => RunOutcome::Terminated {
      message: other.to_string(),
      stdout,
      stderr,
    },
  }
}

/// Future boundary for a TS→native backend.
///
/// This trait is intentionally lightweight: it takes a TypeScript snippet and returns the
/// **captured stdout output** (with at most one trailing newline removed).
///
/// In practice this is the output produced via the harness' minimal "native builtins" prelude:
/// `print(...)` / `console.log(...)`.
///
/// The `vm-js` oracle implements this by running the snippet under the interpreter with a minimal
/// injected builtins prelude, while the `native-js` runner implements it by compiling to a native
/// executable and capturing its stdout.
pub trait NativeRunner {
  fn compile_and_run(&self, ts: &str) -> Result<String, Diagnostic>;
}

/// Like [`NativeRunner`], but returns a structured [`RunOutcome`] instead of a value-only `String`.
///
/// This is intended for robust oracle-vs-native comparisons that need to distinguish between
/// completion, uncaught exceptions, termination, and compile errors.
pub trait NativeRunner2 {
  fn compile_and_run(&self, ts: &str) -> RunOutcome;
}

/// A runner that uses the `vm-js` interpreter as a JavaScript oracle.
pub struct VmJsOracleRunner;

impl VmJsOracleRunner {
  pub fn new() -> Self {
    Self
  }
}

impl Default for VmJsOracleRunner {
  fn default() -> Self {
    Self::new()
  }
}

impl NativeRunner for VmJsOracleRunner {
  fn compile_and_run(&self, ts: &str) -> Result<String, Diagnostic> {
    let js = ts_to_js(ts)?;
    let options = OracleHarnessOptions::default();
    let mut rt = new_runtime_with_options(&options)
      .map_err(|err| harness_error(format!("failed to init vm-js: {err}")))?;
    rt.vm.set_user_data(ConsoleCapture::default());

    let finish_err = |rt: &mut JsRuntime, mut diag: Diagnostic| -> Diagnostic {
      let (stdout, stderr) = take_captured_console(&mut rt.vm);
      teardown_microtasks(rt);
      if !stdout.is_empty() {
        diag.push_note(format!("stdout:\n{stdout}"));
      }
      if !stderr.is_empty() {
        diag.push_note(format!("stderr:\n{stderr}"));
      }
      diag
    };

    if let Err(err) = install_native_builtins(&mut rt) {
      let diag = vm_error_to_diagnostic(&rt, err);
      return Err(finish_err(&mut rt, diag));
    }

    let fixture_source = match SourceText::new_charged_arc(&mut rt.heap, "<fixture>", js) {
      Ok(source) => source,
      Err(err) => {
        let diag = vm_error_to_diagnostic(&rt, err);
        return Err(finish_err(&mut rt, diag));
      }
    };
    if let Err(err) = rt.exec_script_source(fixture_source) {
      let diag = vm_error_to_diagnostic(&rt, err);
      return Err(finish_err(&mut rt, diag));
    }

    if let Err(err) = rt.vm.perform_microtask_checkpoint(&mut rt.heap) {
      let diag = vm_error_to_diagnostic(&rt, err);
      return Err(finish_err(&mut rt, diag));
    }

    let (stdout, _stderr) = take_captured_console(&mut rt.vm);
    teardown_microtasks(&mut rt);
    Ok(stdout)
  }
}

impl NativeRunner2 for VmJsOracleRunner {
  fn compile_and_run(&self, ts: &str) -> RunOutcome {
    run_fixture_ts_outcome(ts)
  }
}

/// Run a TypeScript fixture and return a structured [`RunOutcome`].
///
/// This uses the same "global observation protocol" as [`run_fixture_ts`]: it executes the erased
/// script, performs a microtask checkpoint, then evaluates `String(globalThis.__native_result)`.
pub fn run_fixture_ts_outcome(source: &str) -> RunOutcome {
  run_fixture_ts_outcome_with_name("<fixture>", source)
}

/// Like [`run_fixture_ts_outcome`] but uses a custom source name for VM error reporting.
pub fn run_fixture_ts_outcome_with_name(name: &str, source: &str) -> RunOutcome {
  run_fixture_ts_outcome_with_name_and_options(name, source, &OracleHarnessOptions::default())
}

/// Like [`run_fixture_ts_outcome_with_name`], but allows customizing the VM options and heap limits.
///
/// This is primarily intended for tests that need to trigger deterministic termination conditions
/// (e.g. out-of-fuel).
pub fn run_fixture_ts_outcome_with_name_and_options(
  name: &str,
  source: &str,
  options: &OracleHarnessOptions,
) -> RunOutcome {
  let js = match ts_to_js(source) {
    Ok(js) => js,
    Err(diag) => return RunOutcome::CompileError { diagnostic: diag },
  };

  let mut rt = match new_runtime_with_options(options) {
    Ok(rt) => rt,
    Err(err) => {
      return RunOutcome::Terminated {
        message: format!("failed to init vm-js: {err}"),
        stdout: String::new(),
        stderr: String::new(),
      }
    }
  };
  rt.vm.set_user_data(ConsoleCapture::default());

  let outcome = (|| {
    if let Err(err) = install_native_builtins(&mut rt) {
      return vm_error_to_outcome(&mut rt, err);
    }

    let fixture_source = match SourceText::new_charged_arc(&mut rt.heap, name, js) {
      Ok(source) => source,
      Err(err) => return vm_error_to_outcome(&mut rt, err),
    };
    if let Err(err) = rt.exec_script_source(fixture_source) {
      return vm_error_to_outcome(&mut rt, err);
    }

    if let Err(err) = rt.vm.perform_microtask_checkpoint(&mut rt.heap) {
      return vm_error_to_outcome(&mut rt, err);
    }

    let observe_source =
      match SourceText::new_charged_arc(&mut rt.heap, OBSERVE_SOURCE_NAME, OBSERVE_SCRIPT) {
        Ok(source) => source,
        Err(err) => return vm_error_to_outcome(&mut rt, err),
      };
    let value = match rt.exec_script_source(observe_source) {
      Ok(v) => v,
      Err(err) => return vm_error_to_outcome(&mut rt, err),
    };

    let Value::String(s) = value else {
      return RunOutcome::Terminated {
        message: format!("observe script returned non-string value: {value:?}"),
        stdout: String::new(),
        stderr: String::new(),
      };
    };

    let value = match rt.heap().get_string(s) {
      Ok(s) => s.to_utf8_lossy(),
      Err(err) => {
        return RunOutcome::Terminated {
          message: format!("invalid string handle: {err}"),
          stdout: String::new(),
          stderr: String::new(),
        }
      }
    };

    RunOutcome::Ok {
      value,
      stdout: String::new(),
      stderr: String::new(),
    }
  })();

  let (stdout, stderr) = take_captured_console(&mut rt.vm);
  teardown_microtasks(&mut rt);
  attach_stdio(outcome, stdout, stderr)
}

/// A [`NativeRunner`] implementation backed by the `native-js` AOT compiler.
///
/// ## Script-vs-module wrapper
///
/// `native-js`'s `compile_typescript_to_llvm_ir` parses sources as ES modules (`SourceType::Module`).
/// Many fixtures (and small snippets) are authored as scripts without any top-level `import`/`export`
/// declarations. To keep the runner deterministic and avoid module-detection mismatches, the runner
/// appends `export {};` to inputs that do not already contain any top-level import/export markers.
///
/// `export {};` is a runtime no-op but forces TypeScript to treat a file as a module.
#[cfg(feature = "native-js-runner")]
#[derive(Debug, Clone)]
pub struct NativeJsRunner {
  pub timeout: std::time::Duration,
  pub opt_level: native_js::OptLevel,
  pub debug: bool,
}

#[cfg(feature = "native-js-runner")]
impl NativeJsRunner {
  pub fn new() -> Self {
    Self {
      timeout: std::time::Duration::from_secs(3),
      opt_level: native_js::OptLevel::O0,
      debug: false,
    }
  }

  fn is_module_source(ts: &str) -> Option<bool> {
    use parse_js::ast::stmt::Stmt;

    let opts = ParseOptions {
      dialect: Dialect::Ts,
      source_type: SourceType::Module,
    };
    let ast = parse_with_options(ts, opts).ok()?;

    for stmt in &ast.stx.body {
      match stmt.stx.as_ref() {
        Stmt::Import(_)
        | Stmt::ExportList(_)
        | Stmt::ExportDefaultExpr(_)
        | Stmt::ExportAssignmentDecl(_)
        | Stmt::ExportAsNamespaceDecl(_)
        | Stmt::ImportTypeDecl(_)
        | Stmt::ExportTypeDecl(_)
        | Stmt::ImportEqualsDecl(_) => return Some(true),
        Stmt::FunctionDecl(func) => {
          if func.stx.export || func.stx.export_default {
            return Some(true);
          }
        }
        Stmt::ClassDecl(class) => {
          if class.stx.export || class.stx.export_default {
            return Some(true);
          }
        }
        Stmt::VarDecl(var) => {
          if var.stx.export {
            return Some(true);
          }
        }
        _ => {}
      }
    }

    Some(false)
  }

  fn wrap_script_as_module(ts: &str) -> String {
    match Self::is_module_source(ts) {
      // Preserve the original source if it already has an import/export marker.
      Some(true) => return ts.to_string(),
      // If parsing fails, don't rewrite it (keeps syntax errors stable).
      None => return ts.to_string(),
      Some(false) => {}
    };

    let mut out = ts.to_string();
    if !out.ends_with('\n') {
      out.push('\n');
    }
    out.push_str("export {};\n");
    out
  }

  fn path_with_suffix(path: &std::path::Path, suffix: &str) -> std::path::PathBuf {
    let mut os = path.as_os_str().to_owned();
    os.push(suffix);
    std::path::PathBuf::from(os)
  }

  fn format_excerpt(bytes: &[u8]) -> String {
    const MAX_LEN: usize = 8 * 1024;
    let s = String::from_utf8_lossy(bytes);
    let mut out = s.into_owned();
    if out.len() > MAX_LEN {
      out.truncate(MAX_LEN);
      out.push_str("\n<output truncated>");
    }
    out
  }

  fn strip_one_trailing_newline(s: &mut String) {
    if s.ends_with('\n') {
      s.pop();
      if s.ends_with('\r') {
        s.pop();
      }
    }
  }

  fn bytes_to_captured_string(bytes: &[u8]) -> String {
    let mut s = String::from_utf8_lossy(bytes).into_owned();
    Self::strip_one_trailing_newline(&mut s);
    s
  }
}

#[cfg(feature = "native-js-runner")]
impl Default for NativeJsRunner {
  fn default() -> Self {
    Self::new()
  }
}

#[cfg(feature = "native-js-runner")]
impl NativeRunner for NativeJsRunner {
  fn compile_and_run(&self, ts: &str) -> Result<String, Diagnostic> {
    use std::io::Read;
    use std::process::Command;
    use std::process::Stdio;
    use wait_timeout::ChildExt;

    let ts = Self::wrap_script_as_module(ts);

    let mut opts = native_js::CompileOptions::default();
    opts.emit = native_js::EmitKind::Executable;
    opts.builtins = true;
    opts.opt_level = self.opt_level;
    opts.debug = self.debug;

    let output = native_js::compiler::compile_typescript_to_artifact(&ts, opts, None).map_err(
      |err| match err {
        native_js::NativeJsError::Parse(parse_err) => parse_err.to_diagnostic(FileId(0)),
        other => {
          if let Some(diags) = other.diagnostics() {
            diagnostics_to_one(diags.to_vec())
          } else {
            harness_error(format!("native-js compilation failed: {other}"))
          }
        }
      },
    )?;

    let exe_path = output.path.clone();
    let llvm_ir_path = self
      .debug
      .then(|| Self::path_with_suffix(&exe_path, ".ll"))
      .filter(|p| p.is_file());

    let mut child = Command::new(&exe_path)
      .stdin(Stdio::null())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .spawn()
      .map_err(|err| harness_error(format!("failed to spawn {}: {err}", exe_path.display())))?;

    let status = match child
      .wait_timeout(self.timeout)
      .map_err(|err| harness_error(format!("failed to wait for native executable: {err}")))?
    {
      Some(status) => status,
      None => {
        let _ = child.kill();
        let _ = child.wait();
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        if let Some(mut out) = child.stdout.take() {
          let _ = out.read_to_end(&mut stdout);
        }
        if let Some(mut err) = child.stderr.take() {
          let _ = err.read_to_end(&mut stderr);
        }

        let mut msg = format!(
          "native executable timed out after {:?}: {}",
          self.timeout,
          exe_path.display()
        );
        if let Some(ll) = llvm_ir_path.as_ref() {
          msg.push_str(&format!("\nllvm ir: {}", ll.display()));
        }
        if !stdout.is_empty() {
          msg.push_str("\nstdout:\n");
          msg.push_str(&Self::format_excerpt(&stdout));
        }
        if !stderr.is_empty() {
          msg.push_str("\nstderr:\n");
          msg.push_str(&Self::format_excerpt(&stderr));
        }
        return Err(harness_error(msg));
      }
    };

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    child
      .stdout
      .take()
      .expect("stdout piped")
      .read_to_end(&mut stdout)
      .map_err(|err| harness_error(format!("failed to read stdout: {err}")))?;
    child
      .stderr
      .take()
      .expect("stderr piped")
      .read_to_end(&mut stderr)
      .map_err(|err| harness_error(format!("failed to read stderr: {err}")))?;

    if !status.success() {
      let mut msg = format!(
        "native executable exited with status {status}: {}",
        exe_path.display()
      );
      if let Some(ll) = llvm_ir_path.as_ref() {
        msg.push_str(&format!("\nllvm ir: {}", ll.display()));
      }
      if !stdout.is_empty() {
        msg.push_str("\nstdout:\n");
        msg.push_str(&Self::format_excerpt(&stdout));
      }
      if !stderr.is_empty() {
        msg.push_str("\nstderr:\n");
        msg.push_str(&Self::format_excerpt(&stderr));
      }
      return Err(harness_error(msg));
    }

    let mut stdout = String::from_utf8(stdout)
      .map_err(|err| harness_error(format!("native stdout was not valid UTF-8: {err}")))?;
    Self::strip_one_trailing_newline(&mut stdout);
    Ok(stdout)
  }
}

#[cfg(feature = "native-js-runner")]
impl NativeRunner2 for NativeJsRunner {
  fn compile_and_run(&self, ts: &str) -> RunOutcome {
    use std::io::Read;
    use std::process::Command;
    use std::process::Stdio;
    use wait_timeout::ChildExt;

    let ts = Self::wrap_script_as_module(ts);

    let mut opts = native_js::CompileOptions::default();
    opts.emit = native_js::EmitKind::Executable;
    opts.builtins = true;
    opts.opt_level = self.opt_level;
    opts.debug = self.debug;

    let output = match native_js::compiler::compile_typescript_to_artifact(&ts, opts, None) {
      Ok(output) => output,
      Err(err) => {
        let diagnostic = match err {
          native_js::NativeJsError::Parse(parse_err) => parse_err.to_diagnostic(FileId(0)),
          other => {
            if let Some(diags) = other.diagnostics() {
              diagnostics_to_one(diags.to_vec())
            } else {
              harness_error(format!("native-js compilation failed: {other}"))
            }
          }
        };
        return RunOutcome::CompileError { diagnostic };
      }
    };

    let exe_path = output.path;

    let mut child = match Command::new(&exe_path)
      .stdin(Stdio::null())
      .stdout(Stdio::piped())
      .stderr(Stdio::piped())
      .spawn()
    {
      Ok(child) => child,
      Err(err) => {
        return RunOutcome::Terminated {
          message: format!("failed to spawn native executable: {err}"),
          stdout: String::new(),
          stderr: String::new(),
        }
      }
    };

    let status = match child.wait_timeout(self.timeout) {
      Ok(Some(status)) => status,
      Ok(None) => {
        let _ = child.kill();
        let _ = child.wait();

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        if let Some(mut out) = child.stdout.take() {
          let _ = out.read_to_end(&mut stdout);
        }
        if let Some(mut err) = child.stderr.take() {
          let _ = err.read_to_end(&mut stderr);
        }

        return RunOutcome::Terminated {
          // Avoid including the temporary executable path so the message is stable for comparisons.
          message: format!("native executable timed out after {:?}", self.timeout),
          stdout: Self::bytes_to_captured_string(&stdout),
          stderr: Self::bytes_to_captured_string(&stderr),
        };
      }
      Err(err) => {
        return RunOutcome::Terminated {
          message: format!("failed to wait for native executable: {err}"),
          stdout: String::new(),
          stderr: String::new(),
        }
      }
    };

    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    if let Some(mut out) = child.stdout.take() {
      if out.read_to_end(&mut stdout).is_err() {
        if let Some(mut err) = child.stderr.take() {
          let _ = err.read_to_end(&mut stderr);
        }
        return RunOutcome::Terminated {
          message: "failed to read native stdout".to_string(),
          stdout: Self::bytes_to_captured_string(&stdout),
          stderr: Self::bytes_to_captured_string(&stderr),
        };
      }
    }
    if let Some(mut err) = child.stderr.take() {
      if err.read_to_end(&mut stderr).is_err() {
        return RunOutcome::Terminated {
          message: "failed to read native stderr".to_string(),
          stdout: Self::bytes_to_captured_string(&stdout),
          stderr: Self::bytes_to_captured_string(&stderr),
        };
      }
    }

    let stdout = Self::bytes_to_captured_string(&stdout);
    let stderr = Self::bytes_to_captured_string(&stderr);

    if !status.success() {
      return RunOutcome::Terminated {
        message: format!("native executable exited with status {status}"),
        stdout,
        stderr,
      };
    }

    RunOutcome::Ok {
      // NativeJsRunner currently only exposes stdout; there is no JS completion value protocol.
      value: "undefined".to_string(),
      stdout,
      stderr,
    }
  }
}

/// Run a TypeScript fixture and return the deterministic observation string.
///
/// Protocol:
/// 1) The TS source is parsed with `parse-js`, erased to JS with `ts-erase`, and emitted as
///    JavaScript via `emit-js`.
/// 2) The emitted JS is executed with `vm-js`.
/// 3) A microtask checkpoint is performed so `.then(...)` callbacks can run.
/// 4) The harness evaluates `String(globalThis.__native_result)` and returns it.
pub fn run_fixture_ts(source: &str) -> Result<String, Diagnostic> {
  run_fixture_ts_with_name("<fixture>", source)
}

/// Like [`run_fixture_ts`] but uses a custom source name for VM error reporting.
pub fn run_fixture_ts_with_name(name: &str, source: &str) -> Result<String, Diagnostic> {
  run_fixture_ts_with_name_and_options(name, source, &OracleHarnessOptions::default())
}

/// Like [`run_fixture_ts_with_name`] but allows configuring the VM/heap budgets via
/// [`OracleHarnessOptions`].
pub fn run_fixture_ts_with_name_and_options(
  name: &str,
  source: &str,
  options: &OracleHarnessOptions,
) -> Result<String, Diagnostic> {
  let js = ts_to_js(source)?;

  let mut rt = new_runtime_with_options(options)
    .map_err(|err| harness_error(format!("failed to init vm-js: {err}")))?;

  let fixture_source = SourceText::new_charged_arc(&mut rt.heap, name, js)
    .map_err(|err| harness_error(format!("{err}")))?;
  if let Err(err) = rt.exec_script_source(fixture_source) {
    let diag = vm_error_to_diagnostic(&rt, err);
    teardown_microtasks(&mut rt);
    return Err(diag);
  }

  if let Err(err) = rt.vm.perform_microtask_checkpoint(&mut rt.heap) {
    let diag = vm_error_to_diagnostic(&rt, err);
    teardown_microtasks(&mut rt);
    return Err(diag);
  }

  let observe_source =
    match SourceText::new_charged_arc(&mut rt.heap, OBSERVE_SOURCE_NAME, OBSERVE_SCRIPT) {
      Ok(source) => source,
      Err(err) => {
        teardown_microtasks(&mut rt);
        return Err(harness_error(format!("{err}")));
      }
    };
  let value = match rt.exec_script_source(observe_source) {
    Ok(v) => v,
    Err(err) => {
      let diag = vm_error_to_diagnostic(&rt, err);
      teardown_microtasks(&mut rt);
      return Err(diag);
    }
  };

  let Value::String(s) = value else {
    let diag = harness_error(format!(
      "observe script returned non-string value: {value:?}"
    ));
    teardown_microtasks(&mut rt);
    return Err(diag);
  };

  let out = rt
    .heap()
    .get_string(s)
    .map_err(|err| harness_error(format!("invalid string handle: {err}")))?
    .to_utf8_lossy();

  teardown_microtasks(&mut rt);
  Ok(out)
}

fn path_to_forward_slash_string(path: &Path) -> String {
  // Create a stable source name for stack traces regardless of platform path separators.
  path
    .components()
    .map(|c| c.as_os_str().to_string_lossy())
    .collect::<Vec<_>>()
    .join("/")
}

fn stringify_value_with_vm_and_heap(
  vm: &mut Vm,
  heap: &mut Heap,
  value: Value,
  depth: usize,
) -> String {
  const MAX_DEPTH: usize = 8;
  if depth >= MAX_DEPTH {
    return format!("{value:?}");
  }

  let res = {
    let mut host = ();
    let mut hooks = MicrotaskQueue::new();
    let mut scope = heap.scope();
    let res = scope.to_string(vm, &mut host, &mut hooks, value);

    // `ToString` can run user code which may queue Promise jobs that hold persistent roots. Ensure
    // we always discard those jobs so we don't leak roots during diagnostic formatting.
    struct TeardownCtx<'a> {
      heap: &'a mut Heap,
    }

    impl VmJobContext for TeardownCtx<'_> {
      fn call(
        &mut self,
        _host: &mut dyn VmHostHooks,
        _callee: Value,
        _this: Value,
        _args: &[Value],
      ) -> Result<Value, VmError> {
        Err(VmError::Unimplemented("TeardownCtx::call"))
      }

      fn construct(
        &mut self,
        _host: &mut dyn VmHostHooks,
        _callee: Value,
        _args: &[Value],
        _new_target: Value,
      ) -> Result<Value, VmError> {
        Err(VmError::Unimplemented("TeardownCtx::construct"))
      }

      fn add_root(&mut self, value: Value) -> Result<RootId, VmError> {
        self.heap.add_root(value)
      }

      fn remove_root(&mut self, id: RootId) {
        self.heap.remove_root(id);
      }
    }

    let mut teardown_ctx = TeardownCtx {
      heap: scope.heap_mut(),
    };
    hooks.teardown(&mut teardown_ctx);
    res
  };

  match res {
    Ok(s) => heap
      .get_string(s)
      .map(|js| js.to_utf8_lossy())
      .unwrap_or_else(|_| "<invalid string>".to_string()),
    Err(VmError::Throw(v) | VmError::ThrowWithStack { value: v, .. }) => {
      stringify_value_with_vm_and_heap(vm, heap, v, depth + 1)
    }
    Err(_) => format!("{value:?}"),
  }
}

struct FixtureModuleLoader {
  root_dir: PathBuf,
  module_ids_by_path: HashMap<PathBuf, ModuleId>,
  module_paths_by_id: HashMap<ModuleId, PathBuf>,
  microtasks: MicrotaskQueue,
}

impl FixtureModuleLoader {
  fn new(root_dir: PathBuf) -> Self {
    Self {
      root_dir,
      module_ids_by_path: HashMap::new(),
      module_paths_by_id: HashMap::new(),
      microtasks: MicrotaskQueue::new(),
    }
  }

  fn register_module_path(&mut self, module: ModuleId, canonical_path: PathBuf) {
    self
      .module_ids_by_path
      .insert(canonical_path.clone(), module);
    self.module_paths_by_id.insert(module, canonical_path);
  }

  fn stable_source_name(&self, canonical_path: &Path) -> Result<String, VmError> {
    let rel = canonical_path.strip_prefix(&self.root_dir).map_err(|_| {
      VmError::InvariantViolation("module path is not under the fixture root directory")
    })?;
    Ok(path_to_forward_slash_string(rel))
  }

  fn base_dir_for_referrer(&self, referrer: ModuleReferrer) -> Result<PathBuf, VmError> {
    match referrer {
      ModuleReferrer::Module(module_id) => {
        let path = self
          .module_paths_by_id
          .get(&module_id)
          .ok_or(VmError::InvariantViolation(
            "module loader missing referrer module path",
          ))?;
        Ok(path.parent().unwrap_or(&self.root_dir).to_path_buf())
      }
      // The harness only supports fixture-relative module specifiers.
      ModuleReferrer::Script(_) | ModuleReferrer::Realm(_) => Ok(self.root_dir.clone()),
    }
  }

  fn teardown(&mut self, heap: &mut Heap) {
    struct TeardownCtx<'a> {
      heap: &'a mut Heap,
    }

    impl VmJobContext for TeardownCtx<'_> {
      fn call(
        &mut self,
        _host: &mut dyn VmHostHooks,
        _callee: Value,
        _this: Value,
        _args: &[Value],
      ) -> Result<Value, VmError> {
        Err(VmError::Unimplemented("TeardownCtx::call"))
      }

      fn construct(
        &mut self,
        _host: &mut dyn VmHostHooks,
        _callee: Value,
        _args: &[Value],
        _new_target: Value,
      ) -> Result<Value, VmError> {
        Err(VmError::Unimplemented("TeardownCtx::construct"))
      }

      fn add_root(&mut self, value: Value) -> Result<RootId, VmError> {
        self.heap.add_root(value)
      }

      fn remove_root(&mut self, id: RootId) {
        self.heap.remove_root(id);
      }
    }

    let mut ctx = TeardownCtx { heap };
    self.microtasks.teardown(&mut ctx);
  }

  fn finish_with_error(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    modules: &mut ModuleGraph,
    referrer: ModuleReferrer,
    module_request: ModuleRequest,
    payload: ModuleLoadPayload,
    err_value: Value,
  ) -> Result<(), VmError> {
    // Root the thrown value for the duration of `FinishLoadingImportedModule`: it can allocate/GC.
    let mut finish_scope = scope.reborrow();
    finish_scope.push_root(err_value)?;
    finish_loading_imported_module(
      vm,
      &mut finish_scope,
      modules,
      self,
      referrer,
      module_request,
      payload,
      Err(VmError::Throw(err_value)),
    )
  }
}

impl VmHostHooks for FixtureModuleLoader {
  fn host_enqueue_promise_job(&mut self, job: Job, realm: Option<RealmId>) {
    self.microtasks.enqueue_promise_job(job, realm);
  }

  fn host_load_imported_module(
    &mut self,
    vm: &mut Vm,
    scope: &mut Scope<'_>,
    modules: &mut ModuleGraph,
    referrer: ModuleReferrer,
    module_request: ModuleRequest,
    host_defined: HostDefined,
    payload: ModuleLoadPayload,
  ) -> Result<(), VmError> {
    let _ = host_defined;

    let base_dir = self.base_dir_for_referrer(referrer)?;
    let specifier = module_request.specifier.to_utf8_lossy();

    let spec_path = Path::new(specifier.as_str());
    if spec_path.is_absolute() {
      let intr = vm
        .intrinsics()
        .ok_or(VmError::Unimplemented("module loading requires intrinsics"))?;
      let err =
        vm_js::new_type_error_object(scope, &intr, "module specifier must be a relative path")?;
      return self.finish_with_error(vm, scope, modules, referrer, module_request, payload, err);
    }
    if spec_path.extension().is_none() {
      let intr = vm
        .intrinsics()
        .ok_or(VmError::Unimplemented("module loading requires intrinsics"))?;
      let err = vm_js::new_type_error_object(
        scope,
        &intr,
        "module specifier must include a file extension",
      )?;
      return self.finish_with_error(vm, scope, modules, referrer, module_request, payload, err);
    }

    let joined = base_dir.join(spec_path);
    let canonical = match fs::canonicalize(&joined) {
      Ok(p) => p,
      Err(err) => {
        let intr = vm
          .intrinsics()
          .ok_or(VmError::Unimplemented("module loading requires intrinsics"))?;
        let msg = format!("failed to resolve module {specifier:?}: {err}");
        let err_value = vm_js::new_error(scope, intr.error_prototype(), "Error", &msg)?;
        return self.finish_with_error(
          vm,
          scope,
          modules,
          referrer,
          module_request,
          payload,
          err_value,
        );
      }
    };

    if !canonical.starts_with(&self.root_dir) {
      let intr = vm
        .intrinsics()
        .ok_or(VmError::Unimplemented("module loading requires intrinsics"))?;
      let msg = "module specifier resolved outside fixture directory";
      let err = vm_js::new_type_error_object(scope, &intr, msg)?;
      return self.finish_with_error(vm, scope, modules, referrer, module_request, payload, err);
    }

    if let Some(id) = self.module_ids_by_path.get(&canonical).copied() {
      return finish_loading_imported_module(
        vm,
        scope,
        modules,
        self,
        referrer,
        module_request,
        payload,
        Ok(id),
      );
    }

    let source = match fs::read_to_string(&canonical) {
      Ok(s) => s,
      Err(err) => {
        let intr = vm
          .intrinsics()
          .ok_or(VmError::Unimplemented("module loading requires intrinsics"))?;
        let msg = format!("failed to read module {}: {err}", canonical.display());
        let err_value = vm_js::new_error(scope, intr.error_prototype(), "Error", &msg)?;
        return self.finish_with_error(
          vm,
          scope,
          modules,
          referrer,
          module_request,
          payload,
          err_value,
        );
      }
    };

    let ext = canonical.extension().and_then(|ext| ext.to_str());
    let js = if matches!(ext, Some("ts") | Some("tsx")) {
      match erase_typescript_to_js_with_source_type(&source, SourceType::Module) {
        Ok(js) => js,
        Err(err) => {
          let intr = vm
            .intrinsics()
            .ok_or(VmError::Unimplemented("module loading requires intrinsics"))?;
          let msg = format!("TS→JS erasure failed for {}: {err}", canonical.display());
          let err_value = vm_js::new_syntax_error_object(scope, &intr, &msg)?;
          return self.finish_with_error(
            vm,
            scope,
            modules,
            referrer,
            module_request,
            payload,
            err_value,
          );
        }
      }
    } else {
      source
    };

    let name = self.stable_source_name(&canonical)?;
    let source_text = SourceText::new_charged_arc(scope.heap_mut(), &name, js)?;

    let record = match SourceTextModuleRecord::parse_source_with_vm(vm, source_text) {
      Ok(record) => record,
      Err(VmError::Syntax(diags)) => {
        let intr = vm
          .intrinsics()
          .ok_or(VmError::Unimplemented("module loading requires intrinsics"))?;
        let diag = diagnostics_to_one(diags);
        let msg = format!("{}: {}", diag.code, diag.message);
        let err_value = vm_js::new_syntax_error_object(scope, &intr, &msg)?;
        return self.finish_with_error(
          vm,
          scope,
          modules,
          referrer,
          module_request,
          payload,
          err_value,
        );
      }
      Err(other) => return Err(other),
    };

    let id = modules.add_module(record)?;
    self.register_module_path(id, canonical);

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

/// Run a directory-based TypeScript fixture as an ECMAScript module graph.
///
/// The fixture directory must contain an entry module named `entry.ts` (or `entry.tsx` / `entry.js`).
/// Other modules are loaded on-demand from the same directory via relative `import` specifiers.
pub fn run_fixture_ts_module_dir(dir: impl AsRef<Path>) -> Result<String, Diagnostic> {
  let dir = dir.as_ref();
  let root_dir = fs::canonicalize(dir).map_err(|err| {
    harness_error(format!(
      "failed to canonicalize fixture dir {}: {err}",
      dir.display()
    ))
  })?;

  let entry_path = ["entry.ts", "entry.tsx", "entry.js"]
    .into_iter()
    .map(|name| root_dir.join(name))
    .find(|p| p.is_file())
    .ok_or_else(|| {
      harness_error(format!(
        "fixture dir {} is missing entry.ts (or entry.tsx / entry.js)",
        root_dir.display()
      ))
    })?;

  let entry_path = fs::canonicalize(&entry_path).map_err(|err| {
    harness_error(format!(
      "failed to canonicalize entry module path {}: {err}",
      entry_path.display()
    ))
  })?;

  if !entry_path.starts_with(&root_dir) {
    return Err(harness_error(format!(
      "entry module {} is outside fixture dir {}",
      entry_path.display(),
      root_dir.display()
    )));
  }

  let entry_name = path_to_forward_slash_string(
    entry_path
      .strip_prefix(&root_dir)
      .expect("already checked starts_with"),
  );

  let options = OracleHarnessOptions::default();
  let mut rt = new_runtime_with_options(&options)
    .map_err(|err| harness_error(format!("failed to init vm-js: {err}")))?;
  let realm_id = rt.realm().id();
  let global_object = rt.realm().global_object();

  let mut loader = FixtureModuleLoader::new(root_dir);

  let exec_result = (|| -> Result<(), Diagnostic> {
    let (vm, modules, heap) = rt.vm_modules_and_heap_mut();

    // Load + parse the entry module.
    let entry_src = fs::read_to_string(&entry_path).map_err(|err| {
      harness_error(format!(
        "failed to read entry module {}: {err}",
        entry_path.display()
      ))
    })?;
    let entry_ext = entry_path.extension().and_then(|ext| ext.to_str());
    let entry_js = if matches!(entry_ext, Some("ts") | Some("tsx")) {
      ts_to_js_with_source_type(&entry_src, SourceType::Module)?
    } else {
      entry_src
    };
    let entry_source_text = SourceText::new_charged_arc(heap, &entry_name, entry_js)
      .map_err(|err| vm_error_to_diagnostic_with_heap(&*heap, err))?;
    let entry_record = SourceTextModuleRecord::parse_source_with_vm(vm, entry_source_text)
      .map_err(|err| vm_error_to_diagnostic_with_heap(&*heap, err))?;
    let entry_id = modules
      .add_module(entry_record)
      .map_err(|err| vm_error_to_diagnostic_with_heap(&*heap, err))?;
    loader.register_module_path(entry_id, entry_path.clone());

    // Load the static module graph.
    let load_promise_value = {
      let mut scope = heap.scope();
      load_requested_modules(
        vm,
        &mut scope,
        modules,
        &mut loader,
        entry_id,
        HostDefined::default(),
      )
      .map_err(|err| vm_error_to_diagnostic_with_heap(scope.heap(), err))?
    };

    let Value::Object(load_promise) = load_promise_value else {
      return Err(harness_error(
        "module graph loading returned a non-object value",
      ));
    };
    if !heap.is_promise(load_promise) {
      return Err(harness_error(
        "module graph loading returned a non-promise object",
      ));
    }

    match heap
      .promise_state(load_promise)
      .map_err(|err| vm_error_to_diagnostic_with_heap(&*heap, err))?
    {
      PromiseState::Fulfilled => {}
      PromiseState::Rejected => {
        let reason = heap
          .promise_result(load_promise)
          .map_err(|err| vm_error_to_diagnostic_with_heap(&*heap, err))?
          .unwrap_or(Value::Undefined);
        let reason_str = stringify_value_with_vm_and_heap(vm, heap, reason, 0);
        return Err(harness_error(format!(
          "module graph loading promise rejected: {reason_str}"
        )));
      }
      PromiseState::Pending => {
        return Err(harness_error(
          "module graph loading promise did not settle (host loader must be synchronous)",
        ));
      }
    }

    // Evaluate the entry module.
    let eval_promise_value = {
      // `ModuleGraph::evaluate` needs an explicit `&mut dyn VmHostHooks`, but we also need `&mut Vm`.
      // Temporarily move out the VM-owned microtask queue so it can serve as the host hooks.
      let mut hooks = mem::take(vm.microtask_queue_mut());
      let mut dummy_host = ();
      let res = modules.evaluate(
        vm,
        heap,
        global_object,
        realm_id,
        entry_id,
        &mut dummy_host,
        &mut hooks,
      );

      // Merge any jobs that were queued directly onto `vm.microtask_queue_mut()` while the queue was
      // temporarily moved out.
      while let Some((realm, job)) = vm.microtask_queue_mut().pop_front() {
        hooks.enqueue_promise_job(job, realm);
      }
      *vm.microtask_queue_mut() = hooks;

      match res {
        Ok(v) => v,
        Err(err) => {
          // `ModuleGraph::evaluate` can return an abrupt completion after starting top-level await
          // evaluation (e.g. if scheduling the resume callbacks fails). Ensure we don't drop the
          // runtime with a pending TLA state holding persistent roots.
          modules.abort_tla_evaluation(vm, heap, entry_id);
          return Err(vm_error_to_diagnostic_with_heap(&*heap, err));
        }
      }
    };

    let Value::Object(eval_promise) = eval_promise_value else {
      return Err(harness_error(
        "module evaluation did not return a promise object",
      ));
    };
    if !heap.is_promise(eval_promise) {
      return Err(harness_error(
        "module evaluation returned a non-promise object",
      ));
    }

    // Root the evaluation promise while running microtasks; it may otherwise be collected.
    let eval_root = heap
      .add_root(Value::Object(eval_promise))
      .map_err(|err| vm_error_to_diagnostic_with_heap(&*heap, err))?;

    // Wait for the evaluation promise to settle, with explicit bounds for determinism.
    let max_checkpoints = OracleHarnessOptions::default().max_microtask_checkpoints;
    let mut checkpoints = 0usize;
    let outcome = loop {
      let state = heap
        .promise_state(eval_promise)
        .map_err(|err| vm_error_to_diagnostic_with_heap(&*heap, err))?;

      match state {
        PromiseState::Pending => {
          if checkpoints >= max_checkpoints {
            modules.abort_tla_evaluation(vm, heap, entry_id);
            break Err(harness_error(format!(
              "module evaluation promise did not settle after {checkpoints} microtask checkpoints"
            )));
          }

          if vm.microtask_queue().is_empty() {
            modules.abort_tla_evaluation(vm, heap, entry_id);
            break Err(harness_error("module evaluation promise did not settle"));
          }

          vm.perform_microtask_checkpoint(heap)
            .map_err(|err| vm_error_to_diagnostic_with_heap(&*heap, err))?;
          checkpoints += 1;
        }
        PromiseState::Fulfilled => break Ok(()),
        PromiseState::Rejected => {
          let reason = heap
            .promise_result(eval_promise)
            .map_err(|err| vm_error_to_diagnostic_with_heap(&*heap, err))?
            .unwrap_or(Value::Undefined);
          let reason_str = stringify_value_with_vm_and_heap(vm, heap, reason, 0);
          break Err(harness_error(format!(
            "module evaluation promise rejected: {reason_str}"
          )));
        }
      }
    };

    // Always remove the root, regardless of success or failure.
    heap.remove_root(eval_root);

    // One final checkpoint (even on rejection) so `.then(...)` / `.catch(...)` callbacks can run and
    // we don't drop queued jobs with persistent roots.
    vm.perform_microtask_checkpoint(heap)
      .map_err(|err| vm_error_to_diagnostic_with_heap(&*heap, err))?;

    outcome?;

    Ok(())
  })();

  // Discard any Promise jobs queued during module graph loading. These jobs can hold persistent
  // roots; dropping them without teardown would leak (and trip debug assertions).
  loader.teardown(&mut rt.heap);

  if let Err(diag) = exec_result {
    teardown_microtasks(&mut rt);
    return Err(diag);
  }

  let observe_source =
    match SourceText::new_charged_arc(&mut rt.heap, OBSERVE_SOURCE_NAME, OBSERVE_SCRIPT) {
      Ok(source) => source,
      Err(err) => {
        let diag = vm_error_to_diagnostic(&rt, err);
        teardown_microtasks(&mut rt);
        return Err(diag);
      }
    };
  let value = match rt.exec_script_source(observe_source) {
    Ok(v) => v,
    Err(err) => {
      let diag = vm_error_to_diagnostic(&rt, err);
      teardown_microtasks(&mut rt);
      return Err(diag);
    }
  };

  let Value::String(s) = value else {
    let diag = harness_error(format!(
      "observe script returned non-string value: {value:?}"
    ));
    teardown_microtasks(&mut rt);
    return Err(diag);
  };

  let out = rt
    .heap()
    .get_string(s)
    .map_err(|err| harness_error(format!("invalid string handle: {err}")))?
    .to_utf8_lossy();

  teardown_microtasks(&mut rt);
  Ok(out)
}

#[cfg(test)]
mod tests {
  use super::{erase_typescript_to_js, erase_typescript_to_js_with_source_type, TsToJsError};
  use std::path::{Path, PathBuf};

  fn fixtures_dir() -> PathBuf {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
      .parent()
      .expect("crate should live under vendor/ecma-rs/")
      .join("fixtures/native_oracle")
  }

  #[test]
  fn fixtures_erase_and_execute_in_oracle() {
    let dir = fixtures_dir();
    let mut fixtures: Vec<PathBuf> = std::fs::read_dir(&dir)
      .unwrap_or_else(|err| panic!("failed to read fixture dir {dir:?}: {err}"))
      .filter_map(|entry| entry.ok().map(|entry| entry.path()))
      .filter(|path| {
        path
          .extension()
          .is_some_and(|ext| ext == "ts" || ext == "tsx")
      })
      .collect();
    fixtures.sort();

    assert!(
      !fixtures.is_empty(),
      "expected at least one fixture under {dir:?}"
    );

    for fixture in fixtures {
      let source = std::fs::read_to_string(&fixture)
        .unwrap_or_else(|err| panic!("failed to read fixture {fixture:?}: {err}"));
      let js = erase_typescript_to_js(&source)
        .unwrap_or_else(|err| panic!("failed to erase fixture {fixture:?}: {err}"));

      let vm = vm_js::Vm::new(vm_js::VmOptions {
        default_fuel: Some(200_000),
        ..vm_js::VmOptions::default()
      });
      let heap = vm_js::Heap::new(vm_js::HeapLimits::new(4 * 1024 * 1024, 2 * 1024 * 1024));
      let mut runtime = vm_js::JsRuntime::new(vm, heap)
        .unwrap_or_else(|err| panic!("failed to create oracle runtime for {fixture:?}: {err:?}"));
      runtime.exec_script(&js).unwrap_or_else(|err| {
        panic!("oracle execution failed for {fixture:?}: {err:?}\nJS:\n{js}")
      });

      // Drain any Promise jobs queued by the fixture so we don't drop `Job` values that still hold
      // persistent roots.
      runtime
        .vm
        .perform_microtask_checkpoint(&mut runtime.heap)
        .unwrap_or_else(|err| {
          panic!("oracle microtask checkpoint failed for {fixture:?}: {err:?}\nJS:\n{js}")
        });
    }
  }

  #[test]
  fn module_erasure_emits_parseable_ecma_module_js() {
    for source in [
      "export const x: number = 1;",
      "import { x } from './dep.ts'; export const y = x + 1;",
    ] {
      let js = erase_typescript_to_js_with_source_type(source, parse_js::SourceType::Module)
        .unwrap_or_else(|err| panic!("failed to erase module source {source:?}: {err}"));

      parse_js::parse_with_options(
        &js,
        parse_js::ParseOptions {
          dialect: parse_js::Dialect::Ecma,
          source_type: parse_js::SourceType::Module,
        },
      )
      .unwrap_or_else(|err| {
        panic!("erased JS should parse as an ECMAScript module: {err}\nJS:\n{js}")
      });
    }
  }

  #[test]
  fn strict_native_erasure_rejects_runtime_ts_constructs() {
    // The harness erases TypeScript using `ts-erase` in strict-native mode, which intentionally
    // rejects runtime TS constructs like enums and namespaces. This keeps the oracle path aligned
    // with the native compiler's strict subset.
    for (label, source) in [
      ("enum", "enum E { A = 1 }"),
      ("namespace", "namespace N { const x = 1; }"),
      ("decorators", "@dec class C {}"),
      ("jsx", "const el = <div>{x}</div>;"),
      ("using", "using x = null;"),
    ] {
      let err = erase_typescript_to_js(source).expect_err("expected strict-native erasure failure");
      let TsToJsError::Erase(diags) = err else {
        panic!("expected TsToJsError::Erase for {label}, got {err:?}");
      };
      assert!(
        diags
          .iter()
          .any(|diag| diag.code.as_str() == "MINIFYTS0001"),
        "expected MINIFYTS0001 diagnostic for {label}, got: {diags:?}"
      );
    }
  }

  #[cfg(feature = "optimize-js-fallback")]
  #[test]
  fn optimize_js_fallback_emits_parseable_js() {
    // Sanity check that the optimize-js compile+decompile fallback produces runnable JS output.
    //
    // The primary TS→JS path (parse → ts-erase → emit-js) is expected to handle most fixtures.
    // This fallback is only intended for syntax that is not supported by the lightweight erasure
    // pipeline yet.
    let source = "switch(1){case 1:break;}";
    let js = super::erase_with_optimize_js_fallback(
      source,
      parse_js::SourceType::Script,
      super::TsToJsError::Optimize(Vec::new()),
    )
    .expect("erase via optimize-js fallback");

    parse_js::parse_with_options(
      &js,
      parse_js::ParseOptions {
        dialect: parse_js::Dialect::Ecma,
        source_type: parse_js::SourceType::Script,
      },
    )
    .expect("fallback JS should parse as strict ECMAScript");

    let vm = vm_js::Vm::new(vm_js::VmOptions::default());
    let heap = vm_js::Heap::new(vm_js::HeapLimits::new(4 * 1024 * 1024, 2 * 1024 * 1024));
    let mut runtime = vm_js::JsRuntime::new(vm, heap).expect("create oracle runtime");
    runtime.vm.set_budget(vm_js::Budget::unlimited(1000));
    runtime
      .exec_script(&js)
      .unwrap_or_else(|err| panic!("execute fallback JS: {err:?}\nJS:\n{js}"));
  }
}
