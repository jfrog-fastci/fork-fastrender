#![cfg(feature = "native-js-runner")]

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Arc;
use std::time::Duration;

use native_js::toolchain::LlvmToolchain;
use native_js::{compile_typescript_to_llvm_ir, CompileOptions};
use native_oracle_harness::erase_typescript_to_js;
use vm_js::{
  GcObject, Heap, HeapLimits, JsRuntime, NativeCall, PropertyDescriptor, PropertyKey, PropertyKind,
  SourceText, Value, Vm, VmError, VmHost, VmHostHooks, VmOptions,
};
use wait_timeout::ChildExt;

#[derive(Default)]
struct ConsoleCaptureHost {
  stdout: String,
}

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

fn console_log_call(
  vm: &mut Vm,
  scope: &mut vm_js::Scope<'_>,
  host: &mut dyn VmHost,
  hooks: &mut dyn VmHostHooks,
  _callee: GcObject,
  _this: Value,
  args: &[Value],
) -> Result<Value, VmError> {
  let host = host
    .as_any_mut()
    .downcast_mut::<ConsoleCaptureHost>()
    .expect("native_compare should run with ConsoleCaptureHost");

  for (idx, arg) in args.iter().copied().enumerate() {
    if idx != 0 {
      host.stdout.push(' ');
    }
    let s = scope.to_string(vm, host, hooks, arg)?;
    host
      .stdout
      .push_str(&scope.heap().get_string(s)?.to_utf8_lossy());
  }
  host.stdout.push('\n');

  Ok(Value::Undefined)
}

fn install_console_capture(rt: &mut JsRuntime) -> Result<(), VmError> {
  let (vm, realm, heap) = rt.vm_realm_and_heap_mut();

  let call_id: vm_js::NativeFunctionId = vm.register_native_call(console_log_call as NativeCall)?;

  let mut scope = heap.scope();
  let log_name = scope.alloc_string("log")?;
  let log_fn = scope.alloc_native_function(call_id, None, log_name, 1)?;
  // Root `log_fn` across allocations while defining it as an object property.
  scope.push_root(Value::Object(log_fn))?;

  let console = scope.alloc_object()?;
  // Root `console` across allocations while defining it on the global object.
  scope.push_root(Value::Object(console))?;

  let log_key = PropertyKey::from_string(scope.alloc_string("log")?);
  scope.define_property(console, log_key, data_desc(Value::Object(log_fn)))?;

  let console_key = PropertyKey::from_string(scope.alloc_string("console")?);
  scope.define_property(
    realm.global_object(),
    console_key,
    data_desc(Value::Object(console)),
  )?;

  Ok(())
}

fn run_oracle_stdout(ts_source_name: &str, ts_source: &str) -> Result<String, String> {
  let js = erase_typescript_to_js(ts_source).map_err(|err| err.to_string())?;

  let vm = Vm::new(VmOptions {
    default_fuel: Some(200_000),
    ..VmOptions::default()
  });
  let heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap).map_err(|err| format!("failed to create vm-js runtime: {err:?}"))?;

  install_console_capture(&mut rt).map_err(|err| format!("failed to install console capture: {err:?}"))?;

  let mut host = ConsoleCaptureHost::default();
  let source = Arc::new(SourceText::new(ts_source_name, js));
  rt.exec_script_source_with_host(&mut host, source)
    .map_err(|err| format!("vm-js execution failed: {err:?}"))?;
  rt.vm
    .perform_microtask_checkpoint_with_host(&mut host, &mut rt.heap)
    .map_err(|err| format!("vm-js microtask checkpoint failed: {err:?}"))?;

  Ok(host.stdout)
}

fn run_with_timeout(mut cmd: Command, timeout: Duration) -> std::io::Result<Output> {
  let mut child = cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()?;
  let mut stdout = Vec::new();
  let mut stderr = Vec::new();
  let mut out_reader = child.stdout.take().expect("stdout piped");
  let mut err_reader = child.stderr.take().expect("stderr piped");

  let status = match child.wait_timeout(timeout)? {
    Some(status) => status,
    None => {
      let _ = child.kill();
      child.wait()?
    }
  };

  out_reader.read_to_end(&mut stdout)?;
  err_reader.read_to_end(&mut stderr)?;

  Ok(Output {
    status,
    stdout,
    stderr,
  })
}

fn run_native_stdout(tc: &LlvmToolchain, ts_source: &str) -> Result<Output, String> {
  let mut opts = CompileOptions::default();
  opts.builtins = true;
  let ir = compile_typescript_to_llvm_ir(ts_source, opts).map_err(|err| err.to_string())?;

  let td = tempfile::tempdir().map_err(|err| format!("failed to create tempdir: {err}"))?;
  let ll_path = td.path().join("out.ll");
  std::fs::write(&ll_path, ir)
    .map_err(|err| format!("failed to write {}: {err}", ll_path.display()))?;

  let exe_path = td.path().join("out");
  let mut cmd = Command::new(&tc.clang);
  cmd
    .arg("-x")
    .arg("ir")
    .arg(&ll_path)
    .arg("-O0")
    .arg("-o")
    .arg(&exe_path);
  let out = run_with_timeout(cmd, Duration::from_secs(30))
    .map_err(|err| format!("failed to invoke clang: {err}"))?;

  if !out.status.success() {
    return Err(format!(
      "clang failed with status {status}\nstdout:\n{stdout}\nstderr:\n{stderr}",
      status = out.status,
      stdout = String::from_utf8_lossy(&out.stdout),
      stderr = String::from_utf8_lossy(&out.stderr)
    ));
  }

  let out = run_with_timeout(Command::new(&exe_path), Duration::from_secs(2))
    .map_err(|err| format!("failed to run {}: {err}", exe_path.display()))?;
  if !out.status.success() {
    return Err(format!(
      "native program exited with status {status}\nstdout:\n{stdout}\nstderr:\n{stderr}",
      status = out.status,
      stdout = String::from_utf8_lossy(&out.stdout),
      stderr = String::from_utf8_lossy(&out.stderr)
    ));
  }

  Ok(out)
}

fn fixtures_dir() -> PathBuf {
  let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
  manifest_dir
    .parent()
    .expect("native-oracle-harness should live under vendor/ecma-rs/")
    .join("fixtures/native_compare")
}

fn normalize_stdout(s: &str) -> &str {
  s.trim_end()
}

#[test]
fn native_compare_fixtures_stdout_matches_oracle() {
  if !cfg!(target_os = "linux") {
    eprintln!("skipping native-compare fixtures: native-js runner is only supported on Linux");
    return;
  }

  let tc = match LlvmToolchain::detect() {
    Ok(tc) => tc,
    Err(err) => {
      eprintln!("skipping native-compare fixtures: {err}");
      return;
    }
  };

  let dir = fixtures_dir();
  let mut fixtures: Vec<PathBuf> = fs::read_dir(&dir)
    .unwrap_or_else(|err| panic!("failed to read fixture dir {}: {err}", dir.display()))
    .filter_map(|entry| entry.ok().map(|entry| entry.path()))
    .filter(|path| matches!(path.extension().and_then(|e| e.to_str()), Some("ts") | Some("tsx")))
    .collect();
  fixtures.sort();

  assert!(
    !fixtures.is_empty(),
    "expected at least one fixture under {}",
    dir.display()
  );

  for path in fixtures {
    let name = path
      .file_name()
      .and_then(|s| s.to_str())
      .unwrap_or("<fixture>");
    let ts =
      fs::read_to_string(&path).unwrap_or_else(|err| panic!("failed to read fixture {name}: {err}"));

    let oracle_raw =
      run_oracle_stdout(name, &ts).unwrap_or_else(|err| panic!("oracle failed for {name}: {err}"));
    let native = run_native_stdout(&tc, &ts).unwrap_or_else(|err| panic!("native failed for {name}: {err}"));

    let oracle = normalize_stdout(&oracle_raw);
    let native_raw = String::from_utf8_lossy(&native.stdout).into_owned();
    let native_norm = normalize_stdout(&native_raw);

    if oracle != native_norm {
      let native_stderr = String::from_utf8_lossy(&native.stderr);
      panic!(
        "native/vm-js stdout mismatch for fixture `{name}`\n\
oracle stdout: {oracle:?}\n\
native stdout: {native_norm:?}\n\
\n\
oracle stdout (raw): {oracle_raw:?}\n\
native stdout (raw): {native_raw:?}\n\
\n\
native stderr:\n{native_stderr}\n\
\n\
TypeScript source:\n{ts}\n"
      );
    }
  }
}

