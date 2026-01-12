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
fn hir_codegen_object_gcptr_field() {
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
        const o = { xs: [1, 2, 3] };
        return o.xs.length;
      }
    "#,
  );
 
  let program = Program::new(host, vec![entry.clone()]);
  let file = program.file_id(&entry).expect("file id");
  let def = program
    .exports_of(file)
    .get("main")
    .and_then(|e| e.def)
    .expect("exported def for `main`");
  let main_sym = native_js::llvm_symbol_for_def(&program, def);
 
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
  assert!(ir.contains("notail call void @rt_write_barrier_gc"), "missing write barrier call:\n{ir}");
 
  let main_ir = function_block(&ir, &format!("@{main_sym}"));
  let store_line = main_ir
    .lines()
    .position(|l| l.contains("store") && l.contains("obj.field"))
    .expect("missing store to object field slot in TS main IR");
  let wb_line = main_ir
    .lines()
    .position(|l| l.contains("@rt_write_barrier_gc"))
    .expect("missing write barrier call in TS main IR");
  assert!(
    store_line < wb_line,
    "expected write barrier to occur after the object field store:\n{main_ir}"
  );
 
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

