//! Test harness for comparing native execution against the `vm-js` oracle.
//!
//! This crate is intentionally small. Its primary job is to:
//! - Load TypeScript fixtures.
//! - Erase TypeScript-only syntax to JavaScript (TS→JS "erasure").
//! - Execute the erased JavaScript in the oracle runtime.
//!
//! The TS→JS step uses the shared `ts-erase` lowering pipeline. When erasure
//! encounters unsupported syntax, consumers can enable the
//! `optimize-js-fallback` feature to fall back to the heavier `optimize-js`
//! compile+decompile path.
//!
//! ## Promise-aware execution
//!
//! Fixtures can return either:
//! - a JavaScript string, or
//! - a `Promise<string>`.
//!
//! When a fixture returns a Promise, the harness performs microtask checkpoints
//! (draining the VM's microtask queue) until the Promise settles, with explicit
//! caps to ensure the harness never hangs.

use std::fs;
use std::path::Path;
use std::sync::Arc;

use emit_js::{EmitOptions, Emitter};
use parse_js::{Dialect, ParseOptions, SourceType};
use vm_js::{format_termination, Heap, HeapLimits, JsRuntime, MicrotaskQueue, PromiseState, SourceText, Value, Vm, VmError, VmOptions};

#[derive(Debug)]
pub enum TsToJsError {
  Parse(parse_js::error::SyntaxError),
  Erase(Vec<diagnostics::Diagnostic>),
  Emit(emit_js::JsEmitError),
  #[cfg(feature = "optimize-js-fallback")]
  Optimize(Vec<optimize_js::Diagnostic>),
  #[cfg(feature = "optimize-js-fallback")]
  OptimizeEmit(optimize_js::ProgramToJsError),
}

impl std::fmt::Display for TsToJsError {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      TsToJsError::Parse(err) => write!(f, "{err}"),
      TsToJsError::Erase(diagnostics) => write!(
        f,
        "ts-erase TS→JS erasure failed with {} diagnostic(s)",
        diagnostics.len()
      ),
      TsToJsError::Emit(err) => write!(f, "emit-js TS→JS erasure failed: {err:?}"),
      #[cfg(feature = "optimize-js-fallback")]
      TsToJsError::Optimize(diagnostics) => write!(
        f,
        "optimize-js TS→JS fallback failed with {} diagnostic(s)",
        diagnostics.len()
      ),
      #[cfg(feature = "optimize-js-fallback")]
      TsToJsError::OptimizeEmit(err) => {
        write!(f, "optimize-js TS→JS fallback emit failed: {err:?}")
      }
    }
  }
}

impl std::error::Error for TsToJsError {}

/// Erase TypeScript-only syntax from `source`, returning JavaScript that can be
/// executed by the oracle VM.
///
/// This is intentionally a best-effort API:
/// - It first attempts to parse TS, erase it via `ts-erase` (strict subset), and
///   emit JS via `emit-js`.
/// - If erasure/emission fails and the `optimize-js-fallback` feature is
///   enabled, it falls back to `optimize-js`'s decompiler, which supports a
///   wider range of syntax (but is significantly heavier).
pub fn erase_typescript_to_js(source: &str) -> Result<String, TsToJsError> {
  let mut ast = parse_js::parse_with_options(
    source,
    ParseOptions {
      dialect: Dialect::Ts,
      source_type: SourceType::Script,
    },
  )
  .map_err(TsToJsError::Parse)?;

  if let Err(diags) = ts_erase::erase_types_strict_native(diagnostics::FileId(0), SourceType::Script, &mut ast) {
    return erase_with_optimize_js_fallback(source, TsToJsError::Erase(diags));
  }

  let mut emitter = Emitter::new(EmitOptions::minified());
  match emit_js::emit_js_top_level(&mut emitter, ast.stx.as_ref()) {
    Ok(()) => Ok(String::from_utf8(emitter.into_bytes()).expect("emitted JS is UTF-8")),
    Err(err) => erase_with_optimize_js_fallback(source, TsToJsError::Emit(err)),
  }
}

#[cfg(feature = "optimize-js-fallback")]
fn erase_with_optimize_js_fallback(source: &str, _original: TsToJsError) -> Result<String, TsToJsError> {
  use optimize_js::{compile_source, program_to_js, DecompileOptions, TopLevelMode};

  let program = compile_source(source, TopLevelMode::Script, false).map_err(TsToJsError::Optimize)?;
  let bytes = program_to_js(&program, &DecompileOptions::default(), EmitOptions::minified())
    .map_err(TsToJsError::OptimizeEmit)?;
  Ok(String::from_utf8(bytes).expect("optimize-js emits UTF-8"))
}

#[cfg(not(feature = "optimize-js-fallback"))]
fn erase_with_optimize_js_fallback(_source: &str, original: TsToJsError) -> Result<String, TsToJsError> {
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

/// Execute a fixture file (TypeScript or JavaScript) and return its output string.
pub fn run_fixture(path: impl AsRef<Path>) -> Result<String, OracleHarnessError> {
  run_fixture_with_options(path, &OracleHarnessOptions::default())
}

pub fn run_fixture_with_options(
  path: impl AsRef<Path>,
  options: &OracleHarnessOptions,
) -> Result<String, OracleHarnessError> {
  let path = path.as_ref();
  let source_name: Arc<str> = path
    .file_name()
    .and_then(|s| s.to_str())
    .unwrap_or("<fixture>")
    .into();
  let source_text = fs::read_to_string(path)?;
  let ext = path.extension().and_then(|ext| ext.to_str());
  if ext == Some("ts") {
    run_typescript_source_with_options(source_name, &source_text, options)
  } else {
    // JS fixtures bypass the TS→JS pipeline; this keeps the promise/microtask
    // corpus independent of `emit-js` feature coverage.
    run_js_source_with_options(source_name, Arc::<str>::from(source_text), options)
  }
}

/// Execute a TypeScript snippet in the oracle VM, returning its output string.
pub fn run_typescript_source_with_options(
  source_name: impl Into<Arc<str>>,
  source_text: &str,
  options: &OracleHarnessOptions,
) -> Result<String, OracleHarnessError> {
  let js = erase_typescript_to_js(source_text)?;
  run_js_source_with_options(source_name, Arc::<str>::from(js), options)
}

/// Execute already-erased JavaScript source, returning its output string.
pub fn run_js_source_with_options(
  source_name: impl Into<Arc<str>>,
  source_text: impl Into<Arc<str>>,
  options: &OracleHarnessOptions,
) -> Result<String, OracleHarnessError> {
  let vm = Vm::new(options.vm_options.clone());
  let heap = Heap::new(options.heap_limits);
  let mut rt = JsRuntime::new(vm, heap).map_err(|e| OracleHarnessError::Vm {
    message: e.to_string(),
  })?;
  let source = Arc::new(SourceText::new(source_name, source_text));

  let result = rt.exec_script_source(source);
  match result {
    Ok(value) => value_to_fixture_string(&mut rt, value, options),
    Err(err) => Err(map_vm_error(&mut rt, err)),
  }
}

fn value_to_fixture_string(
  rt: &mut JsRuntime,
  value: Value,
  options: &OracleHarnessOptions,
) -> Result<String, OracleHarnessError> {
  // Fast path: synchronous string.
  if let Value::String(s) = value {
    return Ok(
      rt
        .heap()
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
          rt
            .heap()
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
        for frame in stack {
          msg.push_str(&format!("{frame}\n"));
        }
        // Remove the trailing newline for cleaner output.
        msg.truncate(msg.trim_end_matches('\n').len());
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
  let res = {
    let mut host = ();
    let mut hooks = MicrotaskQueue::new();
    let mut scope = rt.heap.scope();
    scope.to_string(&mut rt.vm, &mut host, &mut hooks, value)
  };

  match res {
    Ok(s) => rt
      .heap()
      .get_string(s)
      .map(|js| js.to_utf8_lossy())
      .unwrap_or_else(|_| "<invalid string>".to_string()),
    Err(VmError::Throw(v) | VmError::ThrowWithStack { value: v, .. }) => stringify_value(rt, v, depth + 1),
    Err(_) => format!("{value:?}"),
  }
}

#[cfg(test)]
mod tests {
  use super::erase_typescript_to_js;
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
      .filter(|path| path.extension().is_some_and(|ext| ext == "ts"))
      .collect();
    fixtures.sort();

    assert!(
      !fixtures.is_empty(),
      "expected at least one fixture under {dir:?}"
    );

    for fixture in fixtures {
      let source =
        std::fs::read_to_string(&fixture).unwrap_or_else(|err| panic!("failed to read fixture {fixture:?}: {err}"));
      let js = erase_typescript_to_js(&source)
        .unwrap_or_else(|err| panic!("failed to erase fixture {fixture:?}: {err}"));

      let vm = vm_js::Vm::new(vm_js::VmOptions {
        default_fuel: Some(200_000),
        ..vm_js::VmOptions::default()
      });
      let heap = vm_js::Heap::new(vm_js::HeapLimits::new(4 * 1024 * 1024, 2 * 1024 * 1024));
      let mut runtime =
        vm_js::JsRuntime::new(vm, heap).unwrap_or_else(|err| panic!("failed to create oracle runtime for {fixture:?}: {err:?}"));
      runtime
        .exec_script(&js)
        .unwrap_or_else(|err| panic!("oracle execution failed for {fixture:?}: {err:?}\nJS:\n{js}"));

      // Drain any Promise jobs queued by the fixture so we don't drop `Job` values that still hold
      // persistent roots.
      runtime
        .vm
        .perform_microtask_checkpoint(&mut runtime.heap)
        .unwrap_or_else(|err| panic!("oracle microtask checkpoint failed for {fixture:?}: {err:?}\nJS:\n{js}"));
    }
  }

  #[cfg(feature = "optimize-js-fallback")]
  #[test]
  fn optimize_js_fallback_can_handle_emit_js_unsupported_syntax() {
    // `emit-js`'s JS emitter is intentionally minimal; many statement kinds (like function
    // declarations and switch statements) are not supported yet. When the fallback feature is
    // enabled, the harness should be able to produce runnable JS anyway via the `optimize-js`
    // decompiler.
    let source = "switch(1){case 1:break;}";
    let js = erase_typescript_to_js(source).expect("erase via optimize-js fallback");

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
