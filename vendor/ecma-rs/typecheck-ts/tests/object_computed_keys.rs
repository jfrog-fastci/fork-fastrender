use std::sync::Arc;

mod common;

use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{codes, FileKey, MemoryHost, Program};

fn def_by_name(program: &Program, file: FileKey, name: &str) -> typecheck_ts::DefId {
  let file_id = program.file_id(&file).expect("file id");
  program
    .definitions_in_file(file_id)
    .into_iter()
    .find(|def| program.def_name(*def).as_deref() == Some(name))
    .unwrap_or_else(|| panic!("definition {name} not found"))
}

#[test]
fn computed_key_expression_is_checked() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  let source = r#"
export const obj = { [missing]: 1 };
"#;
  let file = FileKey::new("input.ts");
  host.insert(file.clone(), Arc::from(source.to_string()));

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics
      .iter()
      .any(|diag| diag.code.as_str() == codes::UNKNOWN_IDENTIFIER.as_str()),
    "expected unknown identifier diagnostic; got {diagnostics:?}",
  );
}

#[test]
fn constant_computed_key_becomes_property() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  let source = r#"
export const obj = { ["x"]: 1 };
export const y = obj.x;
"#;
  let file = FileKey::new("input.ts");
  host.insert(file.clone(), Arc::from(source.to_string()));

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics: {:?}",
    diagnostics
  );

  let y_def = def_by_name(&program, file.clone(), "y");
  let y_ty = program.type_of_def_interned(y_def);
  assert_eq!(program.display_type(y_ty).to_string(), "number");
}

#[test]
fn constant_computed_key_becomes_property_through_type_assertion() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  let source = r#"
export const obj = { ["x" as string]: 1 };
export const y = obj.x;
"#;
  let file = FileKey::new("input.ts");
  host.insert(file.clone(), Arc::from(source.to_string()));

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics: {:?}",
    diagnostics
  );

  let y_def = def_by_name(&program, file.clone(), "y");
  let y_ty = program.type_of_def_interned(y_def);
  assert_eq!(program.display_type(y_ty).to_string(), "number");
}

#[test]
fn template_literal_computed_key_becomes_property() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  let source = r#"
export const obj = { [`x`]: 1 };
export const y = obj.x;
"#;
  let file = FileKey::new("input.ts");
  host.insert(file.clone(), Arc::from(source.to_string()));

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics: {:?}",
    diagnostics
  );

  let y_def = def_by_name(&program, file.clone(), "y");
  let y_ty = program.type_of_def_interned(y_def);
  assert_eq!(program.display_type(y_ty).to_string(), "number");
}

#[test]
fn computed_key_expression_is_checked_in_const_assertion() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  let source = r#"
export const obj = { [missing]: 1 } as const;
"#;
  let file = FileKey::new("input.ts");
  host.insert(file.clone(), Arc::from(source.to_string()));

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics
      .iter()
      .any(|diag| diag.code.as_str() == codes::UNKNOWN_IDENTIFIER.as_str()),
    "expected unknown identifier diagnostic; got {diagnostics:?}",
  );
}

#[test]
fn constant_computed_key_becomes_property_in_const_assertion() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  let source = r#"
export const obj = { ["x"]: 1 } as const;
export const y = obj.x;
"#;
  let file = FileKey::new("input.ts");
  host.insert(file.clone(), Arc::from(source.to_string()));

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics: {:?}",
    diagnostics
  );

  let y_def = def_by_name(&program, file.clone(), "y");
  let y_ty = program.type_of_def_interned(y_def);
  assert_eq!(program.display_type(y_ty).to_string(), "1");
}

#[test]
fn template_literal_computed_key_becomes_property_in_const_assertion() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  let source = r#"
export const obj = { [`x`]: 1 } as const;
export const y = obj.x;
"#;
  let file = FileKey::new("input.ts");
  host.insert(file.clone(), Arc::from(source.to_string()));

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics: {:?}",
    diagnostics
  );

  let y_def = def_by_name(&program, file.clone(), "y");
  let y_ty = program.type_of_def_interned(y_def);
  assert_eq!(program.display_type(y_ty).to_string(), "1");
}
