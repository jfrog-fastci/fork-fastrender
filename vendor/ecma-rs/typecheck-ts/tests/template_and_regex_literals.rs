use std::sync::Arc;

mod common;

use diagnostics::TextRange;
use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{codes, FileKey, MemoryHost, Program};

#[test]
fn template_literal_has_string_type() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  let file = FileKey::new("entry.ts");
  host.insert(file.clone(), Arc::from("export const s = `hello ${1}`;"));

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");

  let file_id = program.file_id(&file).expect("file id");
  let def = program
    .definitions_in_file(file_id)
    .into_iter()
    .find(|def| program.def_name(*def).as_deref() == Some("s"))
    .expect("definition for s");
  let ty = program.type_of_def(def);
  assert_eq!(program.display_type(ty).to_string(), "string");
}

#[test]
fn regex_literal_has_regexp_type() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  let file = FileKey::new("entry.ts");
  host.insert(file.clone(), Arc::from("export const r = /foo/;"));

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(diagnostics.is_empty(), "unexpected diagnostics: {diagnostics:?}");

  let file_id = program.file_id(&file).expect("file id");
  let def = program
    .definitions_in_file(file_id)
    .into_iter()
    .find(|def| program.def_name(*def).as_deref() == Some("r"))
    .expect("definition for r");
  let ty = program.type_of_def(def);
  assert_eq!(program.display_type(ty).to_string(), "RegExp");
}

#[test]
fn template_substitutions_are_typechecked() {
  let source = "export const bad = `x ${missing}`;";
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  let file = FileKey::new("entry.ts");
  host.insert(file.clone(), Arc::from(source.to_string()));

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();

  let missing_start = source
    .find("missing")
    .expect("expected `missing` in source") as u32;
  let missing_end = missing_start + "missing".len() as u32;
  let missing_range = TextRange::new(missing_start, missing_end);
  assert!(
    diagnostics.iter().any(|d| {
      d.code.as_str() == codes::UNKNOWN_IDENTIFIER.as_str() && d.primary.range == missing_range
    }),
    "expected unknown identifier diagnostic for missing, got {diagnostics:?}"
  );
}

