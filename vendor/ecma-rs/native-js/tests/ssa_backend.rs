use native_js::{compile_program, BackendKind, CompilerOptions, EmitKind};
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
use typecheck_ts::{FileKey, MemoryHost, Program};
use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};
use wait_timeout::ChildExt;

const MAX_CONCURRENT_NATIVE_JS_SSA_TESTS: usize = 4;
static NATIVE_JS_SSA_TESTS_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

struct CodegenPermit;

impl CodegenPermit {
  fn acquire() -> Self {
    loop {
      let current = NATIVE_JS_SSA_TESTS_IN_FLIGHT.load(Ordering::Acquire);
      if current < MAX_CONCURRENT_NATIVE_JS_SSA_TESTS {
        if NATIVE_JS_SSA_TESTS_IN_FLIGHT
          .compare_exchange(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
          .is_ok()
        {
          return Self;
        }
      }
      std::thread::sleep(Duration::from_millis(10));
    }
  }
}

impl Drop for CodegenPermit {
  fn drop(&mut self) {
    NATIVE_JS_SSA_TESTS_IN_FLIGHT.fetch_sub(1, Ordering::Release);
  }
}

fn command_works(cmd: &str) -> bool {
  Command::new(cmd)
    .arg("--version")
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status()
    .is_ok_and(|s| s.success())
}

fn es5_host() -> MemoryHost {
  MemoryHost::with_options(TsCompilerOptions {
    libs: vec![LibName::parse("es5").expect("LibName::parse(es5)")],
    ..Default::default()
  })
}

fn run_vm_js(source: &str) -> Value {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap).expect("failed to create vm-js runtime");
  rt.exec_script(source).expect("vm-js exec")
}

fn compile_and_run(ts_src: &str) -> std::process::Output {
  let mut host = es5_host();
  let key = FileKey::new("main.ts");
  host.insert(key.clone(), ts_src);
  let program = Program::new(host, vec![key.clone()]);
  let diags = program.check();
  assert!(diags.is_empty(), "unexpected type errors: {diags:#?}");
  let entry = program.file_id(&key).unwrap();

  let mut opts = CompilerOptions::default();
  opts.emit = EmitKind::Executable;
  opts.backend = BackendKind::Ssa;
  // Keep the pipeline deterministic and fast for tests; this affects only final object emission.
  opts.opt_level = native_js::OptLevel::O0;

  let artifact = compile_program(&program, entry, &opts).unwrap();
  assert_eq!(artifact.kind, EmitKind::Executable);

  let mut child = Command::new(&artifact.path)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .unwrap();

  let Some(status) = child.wait_timeout(Duration::from_secs(5)).unwrap() else {
    let _ = child.kill();
    let _ = child.wait();
    panic!("compiled executable timed out");
  };

  let mut stdout = Vec::new();
  child.stdout.take().unwrap().read_to_end(&mut stdout).unwrap();
  let mut stderr = Vec::new();
  child.stderr.take().unwrap().read_to_end(&mut stderr).unwrap();

  let out = std::process::Output {
    status,
    stdout,
    stderr,
  };

  let _ = std::fs::remove_file(&artifact.path);
  out
}

fn oracle_exit_code(value: Value) -> i32 {
  match value {
    Value::Number(n) => n as i32,
    Value::Bool(b) => if b { 1 } else { 0 },
    Value::Undefined => 0,
    other => panic!("unsupported oracle value: {other:?}"),
  }
}

#[test]
fn ssa_backend_emits_object_and_passes_statepoint_pipeline() {
  let _permit = CodegenPermit::acquire();

  let ts = r#"
export function main(): number {
  let x: number = 1;
  while (x < 4) {
    x = x + 1;
  }
  return x;
}
"#;

  let mut host = es5_host();
  let key = FileKey::new("main.ts");
  host.insert(key.clone(), ts);
  let program = Program::new(host, vec![key.clone()]);
  let diags = program.check();
  assert!(diags.is_empty(), "unexpected type errors: {diags:#?}");
  let entry = program.file_id(&key).unwrap();

  let mut opts = CompilerOptions::default();
  opts.emit = EmitKind::Object;
  opts.backend = BackendKind::Ssa;
  opts.opt_level = native_js::OptLevel::O0;

  let artifact = compile_program(&program, entry, &opts).unwrap();
  assert_eq!(artifact.kind, EmitKind::Object);
  assert!(artifact.path.exists(), "missing artifact {}", artifact.path.display());

  let bytes = std::fs::read(&artifact.path).unwrap();
  assert!(!bytes.is_empty(), "expected object output to be non-empty");

  let _ = std::fs::remove_file(&artifact.path);
}

#[test]
#[cfg(target_os = "linux")]
fn ssa_backend_arith_if_while_and_direct_call_matches_vm_js() {
  if !command_works("clang-18") && !command_works("clang") {
    eprintln!("skipping: clang not found in PATH");
    return;
  }
  if !command_works("ld.lld-18") && !command_works("ld.lld") {
    eprintln!("skipping: lld not found in PATH");
    return;
  }

  let _permit = CodegenPermit::acquire();

  let ts = r#"
function add(a: number, b: number): number {
  return a + b;
}

export function main(): number {
  let x: number = 1;
  let y: number = 2;
  if (x < y) {
    x = x + 10;
  } else {
    x = x + 20;
  }
  let i: number = 0;
  while (i < 3) {
    x = x + i;
    i = i + 1;
  }
  return add(x, y);
}
"#;

  // Same program without module `export` syntax (vm-js executes scripts, not modules).
  let oracle_js = r#"
function add(a, b) {
  return a + b;
}

function main() {
  let x = 1;
  let y = 2;
  if (x < y) {
    x = x + 10;
  } else {
    x = x + 20;
  }
  let i = 0;
  while (i < 3) {
    x = x + i;
    i = i + 1;
  }
  return add(x, y);
}

main();
"#;

  let expected = oracle_exit_code(run_vm_js(oracle_js));
  let out = compile_and_run(ts);
  assert_eq!(out.status.code(), Some(expected));
  assert!(out.stdout.is_empty());
}

#[test]
#[cfg(target_os = "linux")]
fn ssa_backend_boolean_return_matches_vm_js() {
  if !command_works("clang-18") && !command_works("clang") {
    eprintln!("skipping: clang not found in PATH");
    return;
  }
  if !command_works("ld.lld-18") && !command_works("ld.lld") {
    eprintln!("skipping: lld not found in PATH");
    return;
  }

  let _permit = CodegenPermit::acquire();

  let ts = r#"
export function main(): boolean {
  let x: number = 5;
  let y: number = 6;
  return x < y;
}
"#;

  let oracle_js = r#"
function main() {
  let x = 5;
  let y = 6;
  return x < y;
}
main();
"#;

  let expected = oracle_exit_code(run_vm_js(oracle_js));
  let out = compile_and_run(ts);
  assert_eq!(out.status.code(), Some(expected));
  assert!(out.stdout.is_empty());
}
