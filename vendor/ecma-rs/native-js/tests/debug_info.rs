#![cfg(target_os = "linux")]

use native_js::{compile_program, CompilerOptions, EmitKind, OptLevel};
use object::{Object, ObjectSection};
use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, LibName};
use typecheck_ts::{FileKey, MemoryHost, Program};

fn es5_host() -> MemoryHost {
  MemoryHost::with_options(TsCompilerOptions {
    libs: vec![LibName::parse("es5").expect("LibName::parse(es5)")],
    ..Default::default()
  })
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
  if needle.is_empty() {
    return true;
  }
  haystack.windows(needle.len()).any(|w| w == needle)
}

fn debug_sections_contain<'data>(file: &object::File<'data>, needle: &str) -> bool {
  let needle = needle.as_bytes();
  for section in file.sections() {
    let Ok(name) = section.name() else {
      continue;
    };
    if !name.starts_with(".debug") && !name.starts_with(".zdebug") {
      continue;
    }
    let Ok(data) = section.uncompressed_data() else {
      continue;
    };
    if contains_subslice(data.as_ref(), needle) {
      return true;
    }
  }
  false
}

#[test]
fn debug_single_file_emits_dwarf_and_references_ts_filename() {
  let mut host = es5_host();
  let key = FileKey::new("main.ts");
  host.insert(
    key.clone(),
    r#"
export function main(): number {
  let x = 1;
  let y = 2;
  return x + y;
}
"#,
  );

  let program = Program::new(host, vec![key.clone()]);
  let diags = program.check();
  assert!(diags.is_empty(), "{diags:#?}");
  let entry = program.file_id(&key).unwrap();

  let mut opts = CompilerOptions::default();
  opts.emit = EmitKind::Object;
  opts.debug = true;
  opts.opt_level = OptLevel::O0;

  let artifact = compile_program(&program, entry, &opts).unwrap();
  let bytes = std::fs::read(&artifact.path).unwrap();
  let _ = std::fs::remove_file(&artifact.path);

  let file = object::File::parse(&*bytes).unwrap();

  assert!(
    file.section_by_name(".debug_info").is_some(),
    "expected .debug_info section in debug object"
  );
  assert!(
    file.section_by_name(".debug_line").is_some(),
    "expected .debug_line section in debug object"
  );
  assert!(
    debug_sections_contain(&file, "main.ts"),
    "expected DWARF debug sections to reference original TypeScript filename `main.ts`"
  );
}

#[test]
fn debug_multi_file_emits_dwarf_and_references_all_ts_filenames() {
  let mut host = es5_host();
  let math_key = FileKey::new("math.ts");
  let main_key = FileKey::new("main.ts");

  host.insert(
    math_key.clone(),
    r#"
export function add(a: number, b: number): number {
  return a + b;
}
"#,
  );

  host.insert(
    main_key.clone(),
    r#"
import { add } from "./math.ts";

export function main(): number {
  let x = 1;
  let y = 2;
  return add(x, y);
}
"#,
  );
  host.link(main_key.clone(), "./math.ts", math_key.clone());

  let program = Program::new(host, vec![main_key.clone()]);
  let diags = program.check();
  assert!(diags.is_empty(), "{diags:#?}");
  let entry = program.file_id(&main_key).unwrap();

  let mut opts = CompilerOptions::default();
  opts.emit = EmitKind::Object;
  opts.debug = true;
  opts.opt_level = OptLevel::O0;

  let artifact = compile_program(&program, entry, &opts).unwrap();
  let bytes = std::fs::read(&artifact.path).unwrap();
  let _ = std::fs::remove_file(&artifact.path);

  let file = object::File::parse(&*bytes).unwrap();

  assert!(
    file.section_by_name(".debug_info").is_some(),
    "expected .debug_info section in debug object"
  );
  assert!(
    file.section_by_name(".debug_line").is_some(),
    "expected .debug_line section in debug object"
  );
  assert!(
    debug_sections_contain(&file, "main.ts"),
    "expected DWARF debug sections to reference original TypeScript filename `main.ts`"
  );
  assert!(
    debug_sections_contain(&file, "math.ts"),
    "expected DWARF debug sections to reference original TypeScript filename `math.ts`"
  );
}

#[test]
fn release_object_has_no_dwarf_debug_sections() {
  let mut host = es5_host();
  let key = FileKey::new("main.ts");
  host.insert(key.clone(), "export function main(): number { return 0; }\n");

  let program = Program::new(host, vec![key.clone()]);
  let diags = program.check();
  assert!(diags.is_empty(), "{diags:#?}");
  let entry = program.file_id(&key).unwrap();

  let mut opts = CompilerOptions::default();
  opts.emit = EmitKind::Object;
  opts.debug = false;
  opts.opt_level = OptLevel::O0;

  let artifact = compile_program(&program, entry, &opts).unwrap();
  let bytes = std::fs::read(&artifact.path).unwrap();
  let _ = std::fs::remove_file(&artifact.path);

  let file = object::File::parse(&*bytes).unwrap();
  assert!(
    file.section_by_name(".debug_info").is_none(),
    "expected no .debug_info section in non-debug object"
  );
  assert!(
    file.section_by_name(".debug_line").is_none(),
    "expected no .debug_line section in non-debug object"
  );
}

#[test]
fn debug_info_emits_param_and_local_names() {
  let mut host = es5_host();
  let key = FileKey::new("main.ts");
  host.insert(
    key.clone(),
    r#"
export function add(param_debug_info: number): number {
  let local_debug_info = param_debug_info + 1;
  return local_debug_info;
}

export function main(): number {
  return add(41);
}
"#,
  );

  let program = Program::new(host, vec![key.clone()]);
  let diags = program.check();
  assert!(diags.is_empty(), "{diags:#?}");
  let entry = program.file_id(&key).unwrap();

  let mut opts = CompilerOptions::default();
  opts.emit = EmitKind::Object;
  opts.debug = true;
  opts.opt_level = OptLevel::O0;

  let artifact = compile_program(&program, entry, &opts).unwrap();
  let bytes = std::fs::read(&artifact.path).unwrap();
  let _ = std::fs::remove_file(&artifact.path);

  let file = object::File::parse(&*bytes).unwrap();

  assert!(
    debug_sections_contain(&file, "param_debug_info"),
    "expected DWARF debug sections to contain parameter name `param_debug_info`"
  );
  assert!(
    debug_sections_contain(&file, "local_debug_info"),
    "expected DWARF debug sections to contain local variable name `local_debug_info`"
  );
}

#[test]
fn debug_optimized_object_emission_does_not_crash_with_dbg_value_in_ir() {
  // This is a regression test for a crash observed in LLVM 18's GC/statepoint pipeline when
  // `llvm.dbg.*` intrinsics are present. native-js strips those calls before running
  // `place-safepoints` / `rewrite-statepoints-for-gc`, but should still be able to emit a debug
  // object with working DWARF line tables.
  let mut host = es5_host();
  let key = FileKey::new("main.ts");
  host.insert(
    key.clone(),
    r#"
export function main(): number {
  let x = 1;
  x = x + 1;
  return x;
}
"#,
  );

  let program = Program::new(host, vec![key.clone()]);
  let diags = program.check();
  assert!(diags.is_empty(), "{diags:#?}");
  let entry = program.file_id(&key).unwrap();

  let td = tempfile::tempdir().expect("tempdir");
  let obj_path = td.path().join("out.o");
  let ir_path = td.path().join("out.ll");

  let mut opts = CompilerOptions::default();
  opts.emit = EmitKind::Object;
  opts.debug = true;
  // `native-js` treats `debug=true` + `opt_level=default()` as an implied debug build and forces
  // `O0` unless the caller explicitly requests a non-default optimization level. Pick `O1` so we
  // still exercise the "optimized debug info" path (`llvm.dbg.value`) without paying the full cost
  // of higher optimization levels in tests.
  opts.opt_level = OptLevel::O1;
  opts.output = Some(obj_path.clone());
  opts.emit_ir = Some(ir_path.clone());

  let artifact = compile_program(&program, entry, &opts).unwrap();

  // Ensure the frontend actually produced dbg.value intrinsics (even though they are stripped
  // before the GC/statepoint pipeline runs).
  let ir = std::fs::read_to_string(&ir_path).expect("read IR");
  assert!(
    ir.contains("@llvm.dbg.value"),
    "expected IR to contain llvm.dbg.value, got:\n{ir}"
  );

  let bytes = std::fs::read(&artifact.path).unwrap();
  let _ = std::fs::remove_file(&artifact.path);

  let file = object::File::parse(&*bytes).unwrap();
  assert!(
    file.section_by_name(".debug_info").is_some(),
    "expected .debug_info section in debug object"
  );
  assert!(
    file.section_by_name(".debug_line").is_some(),
    "expected .debug_line section in debug object"
  );
  assert!(
    debug_sections_contain(&file, "main.ts"),
    "expected DWARF debug sections to reference original TypeScript filename `main.ts`"
  );
}
