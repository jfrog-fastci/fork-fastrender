use native_js::{compile_program, CompilerOptions, EmitKind};
use runtime_native_abi::RtGcPrefix;
use std::io::Read;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
use typecheck_ts::{FileKey, MemoryHost, Program};

static NATIVE_JS_OBJECT_TESTS_IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);
const MAX_CONCURRENT_NATIVE_JS_OBJECT_TESTS: usize = 4;

struct CodegenPermit;

impl CodegenPermit {
  fn acquire() -> Self {
    loop {
      let current = NATIVE_JS_OBJECT_TESTS_IN_FLIGHT.load(Ordering::Acquire);
      if current < MAX_CONCURRENT_NATIVE_JS_OBJECT_TESTS {
        if NATIVE_JS_OBJECT_TESTS_IN_FLIGHT
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
    NATIVE_JS_OBJECT_TESTS_IN_FLIGHT.fetch_sub(1, Ordering::Release);
  }
}

fn es5_host() -> MemoryHost {
  MemoryHost::with_options(TsCompilerOptions {
    libs: vec![LibName::parse("es5").expect("LibName::parse(es5)")],
    ..Default::default()
  })
}

fn command_works(cmd: &str) -> bool {
  Command::new(cmd)
    .arg("--version")
    .output()
    .is_ok_and(|out| out.status.success())
}

fn function_block(ir: &str, needle: &str) -> String {
  let mut out = Vec::new();
  let mut in_func = false;

  for line in ir.lines() {
    if !in_func && line.contains(needle) {
      in_func = true;
    }

    if in_func {
      out.push(line);
      if line.trim() == "}" {
        break;
      }
    }
  }

  assert!(in_func, "function block not found (needle={needle}):\n{ir}");
  out.join("\n")
}

#[test]
#[cfg(target_os = "linux")]
fn native_objects_emit_and_register_shape_table_and_use_write_barrier() {
  if !command_works("clang-18") && !command_works("clang") {
    eprintln!("skipping: clang not found in PATH");
    return;
  }
  if !command_works("ld.lld-18") && !command_works("ld.lld") {
    eprintln!("skipping: lld not found in PATH");
    return;
  }

  let _permit = CodegenPermit::acquire();

  let mut host = es5_host();
  let key = FileKey::new("main.ts");
  host.insert(
    key.clone(),
    r#"
type Obj = {
  n: number;
  arr: number[];
  s: string;
};

export function main(): number {
  const o: Obj = { n: 1, arr: [1], s: "hello" };
  o.s = "world";
  o.arr = [2, 3];
  return o.n + o.arr.length;
}
"#,
  );

  let program = Program::new(host, vec![key.clone()]);
  let diags = program.check();
  assert!(diags.is_empty(), "unexpected type errors: {diags:#?}");
  let entry = program.file_id(&key).unwrap();

  let tmp = tempfile::tempdir().unwrap();
  let ll_path = tmp.path().join("out.ll");

  let mut opts = CompilerOptions::default();
  opts.emit = EmitKind::Executable;
  opts.emit_ir = Some(ll_path.clone());

  let artifact = compile_program(&program, entry, &opts).unwrap();
  assert_eq!(artifact.kind, EmitKind::Executable);

  // Smoke-run the produced executable.
  use std::process::Stdio;
  use wait_timeout::ChildExt;
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

  let mut stdout = String::new();
  child.stdout.take().unwrap().read_to_string(&mut stdout).unwrap();
  let mut stderr = String::new();
  child.stderr.take().unwrap().read_to_string(&mut stderr).unwrap();
  assert_eq!(
    status.code(),
    Some(3),
    "unexpected exit status {status:?} stdout={stdout:?} stderr={stderr:?}"
  );

  // Inspect emitted IR:
  // - shape table globals exist
  // - C `main` registers the table
  // - pointer field stores call the runtime write barrier
  let ir = std::fs::read_to_string(&ll_path).unwrap();
  assert!(
    ir.contains("@__nativejs_shape_table"),
    "expected shape table global in IR:\n{ir}"
  );

  // Ensure the GC pointer map includes only the array pointer field (`arr`) and not the string field
  // (`s`), which is represented as a runtime-native interned id (`u32`).
  let header_size: u32 = std::mem::size_of::<RtGcPrefix>()
    .try_into()
    .expect("RtGcPrefix size fits in u32");
  let expected_ptr_offsets = format!("[1 x i32] [i32 {header_size}]");
  assert!(
    ir.contains(&expected_ptr_offsets),
    "expected a single pointer offset for the object shape (header size={header_size}):\n{ir}"
  );

  let main_ir = function_block(&ir, "define i32 @main(");
  assert!(
    main_ir.contains("@rt_register_shape_table"),
    "expected C main wrapper to register shape table:\n{main_ir}"
  );

  assert!(
    ir.contains("rt_write_barrier"),
    "expected a pointer field store to emit a write barrier call:\n{ir}"
  );

  let _ = std::fs::remove_file(&artifact.path);
}
