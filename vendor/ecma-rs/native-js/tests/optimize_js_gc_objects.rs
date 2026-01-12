use native_js::{compile, BackendKind, CompilerOptions as NativeCompilerOptions, EmitKind};
use std::process::{Command, Stdio};
use std::time::Duration;
use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
use typecheck_ts::{FileKey, MemoryHost, Program};
use wait_timeout::ChildExt;

fn cmd_works(cmd: &str) -> bool {
  Command::new(cmd)
    .arg("--version")
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status()
    .is_ok_and(|s| s.success())
}

fn clang_available() -> bool {
  cmd_works("clang-18") || cmd_works("clang")
}

fn lld_available() -> bool {
  cmd_works("ld.lld-18") || cmd_works("ld.lld")
}

#[test]
fn optimize_js_emits_shape_table_and_object_alloc() {
  let mut host = MemoryHost::with_options(TsCompilerOptions {
    // Keep lib set minimal for test speed.
    libs: vec![LibName::parse("es5").expect("LibName::parse(es5)")],
    ..Default::default()
  });
  let entry = FileKey::new("entry.ts");
  host.insert(
    entry.clone(),
    r#"
      export function main(): number {
        const o = { s: "hello" };
        if (o.s === "hello") {
          return 1;
        }
        return 0;
      }
    "#,
  );

  let program = Program::new(host, vec![entry.clone()]);

  let tmp = tempfile::tempdir().expect("tempdir");
  let out_path = tmp.path().join("out.ll");

  let mut opts = NativeCompilerOptions::default();
  opts.backend = BackendKind::Ssa;
  opts.emit = EmitKind::LlvmIr;
  opts.output = Some(out_path.clone());

  let out = compile(&program, &opts).expect("compile");
  let ir = out.llvm_ir.expect("expected llvm_ir");

  assert!(
    ir.contains("@__nativejs_shape_table"),
    "IR missing __nativejs_shape_table global:\n{ir}"
  );
  assert!(
    ir.contains("call void @rt_register_shape_table"),
    "IR missing rt_register_shape_table call:\n{ir}"
  );
  // `RuntimeAbi` lowers may-GC allocators via indirect calls; the callsite still materializes a
  // function pointer slot named `rt.fp.rt_alloc`.
  assert!(
    ir.contains("rt.fp.rt_alloc"),
    "IR missing rt_alloc call machinery:\n{ir}"
  );
}

#[test]
#[cfg(target_os = "linux")]
fn optimize_js_gc_stress_does_not_crash_or_hang() {
  if !clang_available() {
    eprintln!("skipping: clang not found in PATH (expected `clang-18` or `clang`)");
    return;
  }
  if !lld_available() {
    eprintln!("skipping: lld not found in PATH (expected `ld.lld-18` or `ld.lld`)");
    return;
  }

  let mut host = MemoryHost::with_options(TsCompilerOptions {
    libs: vec![LibName::parse("es5").expect("LibName::parse(es5)")],
    ..Default::default()
  });
  let entry = FileKey::new("entry.ts");
  host.insert(
    entry.clone(),
    r#"
      export function main(): number {
        let i = 0;
        let sum = 0;
        while (i < 200000) {
          const o = { x: i };
          // Keep the allocated object live and exercise field loads.
          sum = (sum + o.x) % 1000;
          i = i + 1;
        }
        return sum;
      }
    "#,
  );

  let program = Program::new(host, vec![entry.clone()]);

  let tmp = tempfile::tempdir().expect("tempdir");
  let exe_path = tmp.path().join("out");
  let ir_path = tmp.path().join("out.ll");

  let mut opts = NativeCompilerOptions::default();
  opts.backend = BackendKind::Ssa;
  opts.emit = EmitKind::Executable;
  opts.debug = false;
  opts.output = Some(exe_path.clone());
  opts.emit_ir = Some(ir_path);

  let out = compile(&program, &opts).expect("compile");

  let mut child = Command::new(&out.artifact)
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .spawn()
    .expect("spawn");

  let Some(status) = child.wait_timeout(Duration::from_secs(10)).unwrap() else {
    let _ = child.kill();
    let _ = child.wait();
    panic!("compiled executable timed out");
  };

  assert_eq!(
    status.code(),
    Some(0),
    "expected exit code 0, got {status:?}"
  );
}
