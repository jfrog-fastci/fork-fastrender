mod common;

use std::sync::Arc;

use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{codes, FileKey, MemoryHost, Program};

#[test]
fn inherited_base_constructor_params_and_derived_return_type() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());

  let file = FileKey::new("file0.ts");
  let source = r#"
class Base { constructor(x: number) {} static y: number = 1; }
class Derived extends Base { extra: string = "ok"; }
const d = new Derived(1);
d.extra;
const sy = Derived.y;
"#;
  host.insert(file.clone(), Arc::from(source.to_string()));

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");

  let file_id = program.file_id(&file).expect("file id");
  let d_def = program
    .definitions_in_file(file_id)
    .into_iter()
    .find(|def| program.def_name(*def).as_deref() == Some("d"))
    .expect("def for d");
  assert_eq!(
    program.display_type(program.type_of_def(d_def)).to_string(),
    "Derived"
  );

  let extra_offset = source.find("d.extra").expect("offset for d.extra") as u32 + "d.".len() as u32;
  let extra_ty = program.type_at(file_id, extra_offset).expect("type at d.extra");
  assert_eq!(program.display_type(extra_ty).to_string(), "string");

  let y_offset =
    source.find("Derived.y").expect("offset for Derived.y") as u32 + "Derived.".len() as u32;
  let y_ty = program.type_at(file_id, y_offset).expect("type at Derived.y");
  assert_eq!(program.display_type(y_ty).to_string(), "number");
}

#[test]
fn base_constructors_do_not_apply_when_derived_declares_ctor_arity_mismatch() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());

  let file = FileKey::new("main.ts");
  let source = r#"
class Base { constructor(x: number) {} }
class Derived extends Base { constructor() { super(1); } }
new Derived(1);
"#;
  host.insert(file.clone(), Arc::from(source.to_string()));

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics
      .iter()
      .any(|diag| diag.code.as_str() == codes::NO_OVERLOAD.as_str()),
    "expected NO_OVERLOAD diagnostic, got {diagnostics:?}"
  );
}

#[test]
fn base_constructors_do_not_apply_when_derived_declares_ctor_type_mismatch() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());

  let file = FileKey::new("main.ts");
  let source = r#"
class Base { constructor(x: number) {} }
class Derived extends Base { constructor(x: string) { super(0); } }
new Derived(1);
"#;
  host.insert(file.clone(), Arc::from(source.to_string()));

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics
      .iter()
      .any(|diag| diag.code.as_str() == codes::ARGUMENT_TYPE_MISMATCH.as_str()),
    "expected ARGUMENT_TYPE_MISMATCH diagnostic, got {diagnostics:?}"
  );
}

#[test]
fn inherited_constructors_work_across_files() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());

  let base = FileKey::new("base.ts");
  host.insert(
    base.clone(),
    Arc::from(r#"export class Base { constructor(x: number) {} static y: number = 1; }"#),
  );

  let entry = FileKey::new("entry.ts");
  let source = r#"
import { Base } from "./base";
class Derived extends Base { extra: string = "ok"; }
const d = new Derived(1);
d.extra;
const sy = Derived.y;
"#;
  host.insert(entry.clone(), Arc::from(source.to_string()));
  host.link(entry.clone(), "./base", base.clone());

  let program = Program::new(host, vec![entry.clone()]);
  let diagnostics = program.check();
  assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");

  let file_id = program.file_id(&entry).expect("file id");
  let d_def = program
    .definitions_in_file(file_id)
    .into_iter()
    .find(|def| program.def_name(*def).as_deref() == Some("d"))
    .expect("def for d");
  assert_eq!(
    program.display_type(program.type_of_def(d_def)).to_string(),
    "Derived"
  );

  let extra_offset = source.find("d.extra").expect("offset for d.extra") as u32 + "d.".len() as u32;
  let extra_ty = program.type_at(file_id, extra_offset).expect("type at d.extra");
  assert_eq!(program.display_type(extra_ty).to_string(), "string");

  let y_offset =
    source.find("Derived.y").expect("offset for Derived.y") as u32 + "Derived.".len() as u32;
  let y_ty = program.type_at(file_id, y_offset).expect("type at Derived.y");
  assert_eq!(program.display_type(y_ty).to_string(), "number");
}
