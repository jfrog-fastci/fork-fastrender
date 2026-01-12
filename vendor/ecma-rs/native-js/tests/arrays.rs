use native_js::{compile_program, CompilerOptions, EmitKind};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tempfile::tempdir;
use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
use typecheck_ts::{FileKey, MemoryHost, Program};

const MAX_CONCURRENT_NATIVE_JS_ARRAY_TESTS: usize = 4;
static NATIVE_JS_ARRAY_TESTS_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

struct CodegenPermit;

impl CodegenPermit {
  fn acquire() -> Self {
    loop {
      let current = NATIVE_JS_ARRAY_TESTS_IN_FLIGHT.load(Ordering::Acquire);
      if current < MAX_CONCURRENT_NATIVE_JS_ARRAY_TESTS {
        if NATIVE_JS_ARRAY_TESTS_IN_FLIGHT
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
    NATIVE_JS_ARRAY_TESTS_IN_FLIGHT.fetch_sub(1, Ordering::Release);
  }
}

fn es5_host() -> MemoryHost {
  MemoryHost::with_options(TsCompilerOptions {
    libs: vec![LibName::parse("es5").expect("LibName::parse(es5)")],
    ..Default::default()
  })
}

fn clang_available() -> bool {
  for cand in ["clang-18", "clang"] {
    if Command::new(cand)
      .arg("--version")
      .output()
      .is_ok_and(|out| out.status.success())
    {
      return true;
    }
  }
  false
}

fn lld_available() -> bool {
  for cand in ["ld.lld-18", "ld.lld"] {
    if Command::new(cand)
      .arg("--version")
      .output()
      .is_ok_and(|out| out.status.success())
    {
      return true;
    }
  }
  false
}

fn require_executable_emission_or_skip() -> bool {
  if !cfg!(target_os = "linux") {
    eprintln!("skipping native-js array test: executable emission is linux-only");
    return false;
  }
  if !clang_available() {
    eprintln!("skipping native-js array test: clang not found");
    return false;
  }
  if !lld_available() {
    eprintln!("skipping native-js array test: lld not found");
    return false;
  }
  true
}

fn compile_and_run(ts_src: &str) -> std::process::Output {
  let mut host = es5_host();
  let file = FileKey::new("main.ts");
  host.insert(file.clone(), ts_src);

  let program = Program::new(host, vec![file.clone()]);
  let diags = program.check();
  assert!(diags.is_empty(), "unexpected type errors: {diags:#?}");

  let file_id = program.file_id(&file).unwrap();
  let dir = tempdir().unwrap();
  let exe = dir.path().join("out");
  let mut opts = CompilerOptions::default();
  opts.emit = EmitKind::Executable;
  opts.output = Some(exe.clone());
  let artifact = compile_program(&program, file_id, &opts).unwrap();
  assert_eq!(artifact.kind, EmitKind::Executable);
  assert_eq!(artifact.path, exe);

  Command::new(&artifact.path).output().unwrap()
}

#[test]
fn array_indexing_loads() {
  if !require_executable_emission_or_skip() {
    return;
  }

  let _permit = CodegenPermit::acquire();
  let out = compile_and_run("export function main(): number { const xs = [1, 2, 3]; return xs[0] + xs[2]; }");
  assert_eq!(out.status.code(), Some(4));
  assert!(out.stdout.is_empty());
}

#[test]
fn array_length_and_store() {
  if !require_executable_emission_or_skip() {
    return;
  }

  let _permit = CodegenPermit::acquire();
  let out = compile_and_run(
    "export function main(): number { const xs = [1, 2]; xs[0] = 10; return xs.length + xs[0]; }",
  );
  assert_eq!(out.status.code(), Some(12));
  assert!(out.stdout.is_empty());
}

#[test]
fn tuple_is_supported_as_fixed_length_array() {
  if !require_executable_emission_or_skip() {
    return;
  }

  let _permit = CodegenPermit::acquire();
  let out = compile_and_run(
    "export function main(): number { const xs: [number, number] = [1, 2]; return xs[0] + xs[1] + xs.length; }",
  );
  assert_eq!(out.status.code(), Some(5));
  assert!(out.stdout.is_empty());
}

#[test]
fn array_oob_traps() {
  if !require_executable_emission_or_skip() {
    return;
  }

  let _permit = CodegenPermit::acquire();
  let out = compile_and_run("export function main(): number { const xs = [1, 2]; return xs[2]; }");
  assert!(
    !out.status.success(),
    "expected out-of-bounds trap (status={:?})",
    out.status
  );
}

#[test]
fn array_can_be_passed_across_function_boundary() {
  if !require_executable_emission_or_skip() {
    return;
  }

  let _permit = CodegenPermit::acquire();
  let out = compile_and_run(
    "function len(xs: number[]): number { return xs.length; }\n\
     export function main(): number { return len([1,2,3]); }",
  );
  assert_eq!(out.status.code(), Some(3));
  assert!(out.stdout.is_empty());
}

#[test]
fn array_can_be_returned_across_function_boundary() {
  if !require_executable_emission_or_skip() {
    return;
  }

  let _permit = CodegenPermit::acquire();
  let out = compile_and_run(
    "function mk(): number[] { return [1,2]; }\n\
     export function main(): number { return mk().length; }",
  );
  assert_eq!(out.status.code(), Some(2));
  assert!(out.stdout.is_empty());
}

#[test]
fn tuple_can_be_returned_across_function_boundary() {
  if !require_executable_emission_or_skip() {
    return;
  }

  let _permit = CodegenPermit::acquire();
  let out = compile_and_run(
    "function mk(): [number, number] { return [1,2]; }\n\
     export function main(): number { const xs = mk(); return xs[0] + xs[1] + xs.length; }",
  );
  assert_eq!(out.status.code(), Some(5));
  assert!(out.stdout.is_empty());
}
