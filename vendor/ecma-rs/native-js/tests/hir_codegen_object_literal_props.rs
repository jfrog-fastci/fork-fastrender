use native_js::{compile, CompilerOptions as NativeCompilerOptions, EmitKind};
use std::process::{Command, Stdio};
use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
use typecheck_ts::{FileKey, MemoryHost, Program};
 
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
#[cfg(target_os = "linux")]
fn hir_codegen_object_literal_props() {
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
  let entry = FileKey::new("main.ts");
  host.insert(
    entry.clone(),
    r#"
      export function main(): number {
        const p = { x: 1, y: 2 };
        return p.x + p.y;
      }
    "#,
  );
 
  let program = Program::new(host, vec![entry.clone()]);
 
  let tmp = tempfile::tempdir().expect("tempdir");
  let exe_path = tmp.path().join("out");
  let ir_path = tmp.path().join("out.ll");
 
  let mut opts = NativeCompilerOptions::default();
  opts.emit = EmitKind::Executable;
  opts.output = Some(exe_path);
  opts.emit_ir = Some(ir_path);
 
  let out = compile(&program, &opts).expect("compile");
  let ir = out.llvm_ir.expect("expected llvm_ir when emit_ir is set");
 
  assert!(
    ir.contains("call void @rt_register_shape_table"),
    "missing rt_register_shape_table call:\n{ir}"
  );
  assert!(ir.contains("rt.fp.rt_alloc"), "missing rt_alloc call:\n{ir}");
 
  let output = Command::new(&out.artifact)
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .output()
    .expect("run compiled executable");
  assert_eq!(
    output.status.code(),
    Some(3),
    "unexpected exit status stdout={:?} stderr={:?}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
}

