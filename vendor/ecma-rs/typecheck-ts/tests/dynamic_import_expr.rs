use std::sync::Arc;

use diagnostics::TextRange;
use typecheck_ts::{codes, FileKey, MemoryHost, Program, TypeKindSummary};

#[test]
fn dynamic_import_await_resolves_module_namespace() {
  let mut host = MemoryHost::new();
  let main = FileKey::new("main.ts");
  let dep = FileKey::new("dep.ts");

  host.insert(dep.clone(), Arc::from("export const value: number = 1;"));
  let source = r#"
export async function f() {
  const mod = await import("./dep");
  return mod.value;
}
"#;
  host.insert(main.clone(), Arc::from(source));
  host.link(main.clone(), "./dep", dep.clone());

  let program = Program::new(host, vec![main.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "expected no diagnostics, got {diagnostics:?}"
  );

  let file_id = program.file_id(&main).expect("file id");
  let offset = source.find("mod.value").expect("mod.value in source") as u32 + 4;
  let ty = program.type_at(file_id, offset).expect("type at mod.value");
  assert_eq!(program.type_kind(ty), TypeKindSummary::Number);
}

#[test]
fn dynamic_import_argument_type_mismatch() {
  let mut host = MemoryHost::new();
  let file = FileKey::new("main.ts");
  let source = "async function g() { return await import(1); }\n";
  host.insert(file.clone(), Arc::from(source));

  let program = Program::new(host, vec![file.clone()]);
  let file_id = program.file_id(&file).expect("file id");
  let diagnostics = program.check();

  assert_eq!(
    diagnostics.len(),
    1,
    "expected a single diagnostic, got {diagnostics:?}"
  );
  let diag = &diagnostics[0];
  assert_eq!(
    diag.code.as_str(),
    codes::ARGUMENT_TYPE_MISMATCH.as_str(),
    "expected ARGUMENT_TYPE_MISMATCH diagnostic, got {diagnostics:?}"
  );

  let start = source.find('1').expect("numeric literal present in source") as u32;
  let end = start + 1;
  assert_eq!(diag.primary.file, file_id);
  assert_eq!(diag.primary.range, TextRange::new(start, end));
}

#[test]
fn dynamic_import_unresolved_module_string_emits_diagnostic() {
  let mut host = MemoryHost::new();
  let file = FileKey::new("main.ts");
  let source = "async function h() { return await import(\"./missing\"); }\n";
  host.insert(file.clone(), Arc::from(source));

  let program = Program::new(host, vec![file.clone()]);
  let file_id = program.file_id(&file).expect("file id");
  let diagnostics = program.check();

  assert_eq!(
    diagnostics.len(),
    1,
    "expected a single diagnostic, got {diagnostics:?}"
  );
  let diag = &diagnostics[0];
  assert_eq!(
    diag.code.as_str(),
    codes::UNRESOLVED_MODULE.as_str(),
    "expected UNRESOLVED_MODULE diagnostic, got {diagnostics:?}"
  );

  let start = source
    .find("\"./missing\"")
    .expect("module specifier present in source") as u32;
  let end = start + "\"./missing\"".len() as u32;
  let expected = TextRange::new(start, end);
  assert_eq!(diag.primary.file, file_id);
  assert!(
    diag.primary.range.start <= expected.start && diag.primary.range.end >= expected.end,
    "diagnostic span {:?} should cover specifier span {:?}",
    diag.primary.range,
    expected
  );
}

