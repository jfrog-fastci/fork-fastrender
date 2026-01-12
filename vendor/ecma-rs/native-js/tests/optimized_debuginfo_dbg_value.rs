use native_js::{compile_program, CompilerOptions, EmitKind, OptLevel};
use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn optimized_debuginfo_emits_dbg_value_intrinsics() {
  let mut host = MemoryHost::with_options(TsCompilerOptions {
    libs: vec![LibName::parse("es5").expect("LibName::parse(es5)")],
    ..Default::default()
  });
  let key = FileKey::new("main.ts");
  host.insert(
    key.clone(),
    r#"
export function main(): number {
  let x = 1;
  let y = 2;
  x = x && y;
  return x;
}
"#,
  );

  let program = Program::new(host, vec![key.clone()]);
  let entry = program.file_id(&key).expect("file id");

  let mut opts = CompilerOptions::default();
  opts.emit = EmitKind::LlvmIr;
  opts.debug = true;
  opts.opt_level = OptLevel::O2;

  let artifact = compile_program(&program, entry, &opts).expect("compile_program");
  let ir = std::fs::read_to_string(&artifact.path).expect("read IR");

  assert!(
    ir.contains("@llvm.dbg.value"),
    "expected optimized debug info to emit llvm.dbg.value intrinsics, got:\n{ir}"
  );

  let _ = std::fs::remove_file(&artifact.path);
}
