#![cfg(target_os = "linux")]

use native_js::{compile, CompilerOptions, EmitKind, OptLevel};
use object::{Object, ObjectSection};
use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn typechecked_backend_emits_dwarf_line_tables_for_typescript_sources() {
  // Keep the TypeScript standard library surface small to speed up the test.
  let mut host = MemoryHost::with_options(TsCompilerOptions {
    libs: vec![LibName::parse("es5").expect("LibName::parse(es5)")],
    ..Default::default()
  });

  let entry = FileKey::new("entry.ts");
  host.insert(
    entry.clone(),
    r#"
export function main(): number {
  let x = 1;
  return x + 2;
}
"#,
  );

  let program = Program::new(host, vec![entry.clone()]);

  let tmp = tempfile::tempdir().expect("tempdir");
  let obj_path = tmp.path().join("out.o");
  let ir_path = tmp.path().join("out.ll");

  let mut opts = CompilerOptions::default();
  opts.emit = EmitKind::Object;
  opts.opt_level = OptLevel::O0;
  opts.output = Some(obj_path.clone());
  opts.emit_ir = Some(ir_path);
  opts.debug = true;

  let out = compile(&program, &opts).expect("compile");
  let ir = out.llvm_ir.expect("expected llvm_ir when emit_ir is set");

  // `native-js --debug` should emit LLVM debug metadata (compile unit, files, subprograms, and
  // per-instruction locations) so the backend can produce real DWARF.
  assert!(
    ir.contains("DICompileUnit") || ir.contains("!llvm.dbg.cu"),
    "expected IR to contain a DICompileUnit / !llvm.dbg.cu:\n{ir}"
  );
  assert!(
    ir.contains("DISubprogram"),
    "expected IR to contain at least one DISubprogram:\n{ir}"
  );
  assert!(
    ir.contains(", !dbg !"),
    "expected IR to contain at least one instruction-level !dbg location:\n{ir}"
  );
  let obj = std::fs::read(&out.artifact).expect("read object");
  let file = object::File::parse(obj.as_slice()).expect("parse object");

  let debug_info = file
    .section_by_name(".debug_info")
    .expect("object must contain .debug_info when debug is enabled");
  assert!(
    !debug_info.data().expect("read .debug_info").is_empty(),
    ".debug_info should be non-empty"
  );

  let debug_line = file
    .section_by_name(".debug_line")
    .expect("object must contain .debug_line when debug is enabled");
  let debug_line_data = debug_line.data().expect("read .debug_line");
  assert!(
    !debug_line_data.is_empty(),
    ".debug_line should be non-empty"
  );

  // The line table should reference the originating TypeScript file name. With DWARF v4 the file
  // table strings are embedded directly in `.debug_line`.
  assert!(
    debug_line_data
      .windows("entry.ts".len())
      .any(|w| w == b"entry.ts"),
    "expected .debug_line to reference entry.ts"
  );
}
