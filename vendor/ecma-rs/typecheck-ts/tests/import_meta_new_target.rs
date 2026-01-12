use std::sync::Arc;

mod common;

use typecheck_ts::lib_support::{CompilerOptions, FileKind, LibFile};
use typecheck_ts::{codes, FileKey, MemoryHost, Program};

fn lib_file(name: &str, text: &str) -> LibFile {
  LibFile {
    key: FileKey::new(name),
    name: Arc::from(name),
    kind: FileKind::Dts,
    text: Arc::from(text),
  }
}

fn program_with_source(source: &str, extra_libs: Vec<LibFile>) -> Program {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;

  let mut host = MemoryHost::with_options(options);
  host.add_lib(common::core_globals_lib());
  for lib in extra_libs {
    host.add_lib(lib);
  }

  let entry = FileKey::new("entry.ts");
  host.insert(entry.clone(), Arc::from(source.to_string()));

  Program::new(host, vec![entry])
}

#[test]
fn import_expr_visits_module_expression() {
  let program = program_with_source(
    r#"
export const x = import(missing);
"#,
    Vec::new(),
  );

  let diagnostics = program.check();
  assert!(
    diagnostics
      .iter()
      .any(|diag| diag.code.as_str() == codes::UNKNOWN_IDENTIFIER.as_str()),
    "expected unknown identifier diagnostic from import() argument, got {diagnostics:?}"
  );
}

#[test]
fn import_expr_visits_attributes_expression() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;

  let mut host = MemoryHost::with_options(options);
  host.add_lib(common::core_globals_lib());

  let entry = FileKey::new("entry.ts");
  host.insert(
    entry.clone(),
    Arc::from(
      r#"
export const x = import("x", missing);
"#
      .to_string(),
    ),
  );

  let dep = FileKey::new("x.ts");
  host.insert(dep.clone(), "export {};\n");
  host.link(entry.clone(), "x", dep);

  let program = Program::new(host, vec![entry]);
  let diagnostics = program.check();
  assert!(
    diagnostics.iter().any(|diag| {
      diag.code.as_str() == codes::UNKNOWN_IDENTIFIER.as_str()
        && diag.message.contains("missing")
    }),
    "expected unknown identifier diagnostic from import() attributes argument, got {diagnostics:?}"
  );
}

#[test]
fn import_expr_returns_promise_unknown_when_promise_exists() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;

  let mut host = MemoryHost::with_options(options);
  host.add_lib(common::core_globals_lib());
  host.add_lib(lib_file(
    "promise.d.ts",
    r#"
interface Promise<T> {}
declare var Promise: any;
"#,
  ));

  let entry = FileKey::new("entry.ts");
  host.insert(
    entry.clone(),
    Arc::from(
      r#"
declare function takesPromise(x: Promise<unknown>): void;

export function f() {
  takesPromise(import("x"));
}
"#
      .to_string(),
    ),
  );

  let dep = FileKey::new("x.ts");
  host.insert(dep.clone(), "export {};\n");
  host.link(entry.clone(), "x", dep);

  let program = Program::new(host, vec![entry]);

  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics when Promise is available: {diagnostics:?}"
  );
}

#[test]
fn import_expr_returns_promise_unknown_for_non_literal_specifier() {
  let source = r#"
export function f(spec: string) {
  return import(spec);
}
"#;
  let program = program_with_source(
    source,
    vec![lib_file(
      "promise.d.ts",
      r#"
interface Promise<T> {}
declare var Promise: any;
"#,
    )],
  );

  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics: {diagnostics:?}"
  );

  let file_id = program.file_id(&FileKey::new("entry.ts")).expect("file id");
  let offset = source
    .find("import(spec)")
    .expect("import(spec) in source") as u32
    + 1;
  let ty = program.type_at(file_id, offset).expect("type at import(spec)");
  assert_eq!(program.display_type(ty).to_string(), "Promise<unknown>");
}

#[test]
fn import_expr_falls_back_to_unknown_without_promise() {
  let source = r#"
export function f(spec: string) {
  return import(spec);
}
"#;
  let program = program_with_source(source, Vec::new());

  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics: {diagnostics:?}"
  );

  let file_id = program.file_id(&FileKey::new("entry.ts")).expect("file id");
  let offset = source
    .find("import(spec)")
    .expect("import(spec) in source") as u32
    + 1;
  let ty = program.type_at(file_id, offset).expect("type at import(spec)");
  assert_eq!(program.display_type(ty).to_string(), "unknown");
}

#[test]
fn import_meta_is_typed_when_import_meta_interface_exists() {
  let program = program_with_source(
    r#"
export const x: number = import.meta.foo;
"#,
    vec![lib_file(
      "import_meta.d.ts",
      r#"
interface ImportMeta {
  foo: number;
}
"#,
    )],
  );

  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics when ImportMeta is available: {diagnostics:?}"
  );
}

#[test]
fn import_meta_falls_back_to_unknown_without_import_meta_interface() {
  let source = r#"
export const x = import.meta;
"#;
  let program = program_with_source(source, Vec::new());

  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics: {diagnostics:?}"
  );

  let file_id = program.file_id(&FileKey::new("entry.ts")).expect("file id");
  let offset = source
    .find("import.meta")
    .expect("import.meta in source") as u32
    + 1;
  let ty = program.type_at(file_id, offset).expect("type at import.meta");
  assert_eq!(program.display_type(ty).to_string(), "unknown");
}

#[test]
fn new_target_is_typed_when_function_is_available() {
  let program = program_with_source(
    r#"
declare function takesFunction(x: Function): void;

export function f() {
  takesFunction(new.target);
}
"#,
    Vec::new(),
  );

  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics when new.target is passed to Function: {diagnostics:?}"
  );
}

#[test]
fn new_target_falls_back_to_unknown_without_function_type() {
  let mut options = CompilerOptions::default();
  options.no_default_lib = true;

  let mut host = MemoryHost::with_options(options);
  // Intentionally omit `common::core_globals_lib()` so `Function` is not resolvable.
  let entry = FileKey::new("entry.ts");
  let source = r#"
export function f() {
  return new.target;
}
"#;
  host.insert(entry.clone(), Arc::from(source.to_string()));

  let program = Program::new(host, vec![entry.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics: {diagnostics:?}"
  );

  let file_id = program.file_id(&entry).expect("file id");
  let offset = source
    .find("new.target")
    .expect("new.target in source") as u32
    + 1;
  let ty = program.type_at(file_id, offset).expect("type at new.target");
  assert_eq!(program.display_type(ty).to_string(), "unknown");
}
