use native_js::{compile_typescript_to_llvm_ir, CompileOptions};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;
use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};
use wait_timeout::ChildExt;

#[derive(Clone, Debug)]
enum OracleValue {
  Number(f64),
  Bool(bool),
  Undefined,
}

impl OracleValue {
  fn eq_approx(&self, other: &Self) -> bool {
    match (self, other) {
      (OracleValue::Undefined, OracleValue::Undefined) => true,
      (OracleValue::Bool(a), OracleValue::Bool(b)) => a == b,
      (OracleValue::Number(a), OracleValue::Number(b)) => {
        if a.is_nan() && b.is_nan() {
          return true;
        }
        if a.is_infinite() || b.is_infinite() {
          return a == b;
        }
        // Treat integral values as exact comparisons to keep tests strict.
        let a_is_int = a.fract() == 0.0;
        let b_is_int = b.fract() == 0.0;
        if a_is_int && b_is_int {
          return a == b;
        }
        (a - b).abs() <= 1e-9
      }
      _ => false,
    }
  }
}

fn run_vm_js(source: &str) -> Result<OracleValue, VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap).expect("failed to create vm-js runtime");

  let value = rt.exec_script(source)?;
  let oracle = match value {
    Value::Number(n) => OracleValue::Number(n),
    Value::Bool(b) => OracleValue::Bool(b),
    Value::Undefined => OracleValue::Undefined,
    other => panic!("unsupported oracle return value: {other:?}"),
  };
  Ok(oracle)
}

fn find_clang() -> Option<&'static str> {
  for cand in ["clang-18", "clang"] {
    if Command::new(cand)
      .arg("--version")
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .status()
      .is_ok()
    {
      return Some(cand);
    }
  }
  None
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
      // Timed out.
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

fn parse_stdout_value(stdout: &[u8]) -> Result<OracleValue, String> {
  let s = String::from_utf8_lossy(stdout);
  let trimmed = s.trim();
  if trimmed.is_empty() {
    return Err("native program produced no output".to_string());
  }
  if trimmed.contains(char::is_whitespace) {
    return Err(format!(
      "native program output contained whitespace (expected single value): {trimmed:?}"
    ));
  }

  match trimmed {
    "undefined" => Ok(OracleValue::Undefined),
    "true" => Ok(OracleValue::Bool(true)),
    "false" => Ok(OracleValue::Bool(false)),
    "NaN" => Ok(OracleValue::Number(f64::NAN)),
    "Infinity" => Ok(OracleValue::Number(f64::INFINITY)),
    "-Infinity" => Ok(OracleValue::Number(f64::NEG_INFINITY)),
    other => other
      .parse::<f64>()
      .map(OracleValue::Number)
      .map_err(|err| format!("failed to parse native output {other:?} as number: {err}")),
  }
}

fn run_native_ts(source: &str) -> Result<OracleValue, String> {
  let clang =
    find_clang().ok_or_else(|| "clang not found (expected clang-18 or clang)".to_string())?;

  let mut opts = CompileOptions::default();
  opts.builtins = true;

  let ir = compile_typescript_to_llvm_ir(source, opts).map_err(|err| err.to_string())?;

  let td = tempfile::tempdir().map_err(|err| format!("failed to create tempdir: {err}"))?;
  let ll_path = td.path().join("out.ll");
  std::fs::write(&ll_path, ir)
    .map_err(|err| format!("failed to write {}: {err}", ll_path.display()))?;

  let exe_path = td.path().join("out");
  let mut cmd = Command::new(clang);
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

  parse_stdout_value(&out.stdout)
}

fn fixtures_dir() -> PathBuf {
  Path::new(env!("CARGO_MANIFEST_DIR"))
    .join("tests")
    .join("fixtures")
}

fn load_fixture(name: &str) -> (String, String) {
  let dir = fixtures_dir();
  let ts = std::fs::read_to_string(dir.join(format!("{name}.ts")))
    .unwrap_or_else(|err| panic!("failed to read fixture {name}.ts: {err}"));
  let oracle = std::fs::read_to_string(dir.join(format!("{name}.oracle.js")))
    .unwrap_or_else(|err| panic!("failed to read fixture {name}.oracle.js: {err}"));
  (ts, oracle)
}

fn run_fixture(name: &str) {
  let (ts, oracle_js) = load_fixture(name);
  let expected =
    run_vm_js(&oracle_js).unwrap_or_else(|err| panic!("vm-js failed for {name}: {err:?}"));
  let actual =
    run_native_ts(&ts).unwrap_or_else(|err| panic!("native-js failed for {name}: {err}"));

  if !expected.eq_approx(&actual) {
    panic!(
      "oracle mismatch for fixture `{name}`\nexpected: {expected:?}\nactual:   {actual:?}\n\nTypeScript:\n{ts}\n\nOracle JS:\n{oracle_js}\n"
    );
  }
}

#[test]
fn oracle_fixtures() {
  if find_clang().is_none() {
    eprintln!("skipping native-js oracle fixtures: clang not found");
    return;
  }

  // Run sequentially to avoid spawning many concurrent `clang` processes under `cargo test`.
  for name in ["arithmetic", "branching", "looping", "function_calls"] {
    run_fixture(name);
  }
}
