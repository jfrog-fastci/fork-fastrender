mod common;

use diagnostics::Severity;
use typecheck_ts::codes;
use typecheck_ts::lib_support::{CompilerOptions, ScriptTarget};
use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn interface_return_this_substitutes_to_receiver() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    target: ScriptTarget::EsNext,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());

  let file = FileKey::new("entry.ts");
  let src = r#"
interface Fluent { foo(): this; }
declare const f: Fluent;
const x = f.foo();
x.foo();
"#;
  host.insert(file.clone(), src);

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics: {diagnostics:?}"
  );

  let file_id = program.file_id(&file).expect("file id");
  let x_def = program
    .definitions_in_file(file_id)
    .into_iter()
    .find(|def| program.def_name(*def).as_deref() == Some("x"))
    .expect("definition for x");

  assert_eq!(
    program.display_type(program.type_of_def(x_def)).to_string(),
    "Fluent"
  );
}

#[test]
fn class_derived_fluent_method_substitutes_to_derived() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    target: ScriptTarget::EsNext,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());

  let file = FileKey::new("entry.ts");
  let src = r#"
class Base { clone(): this { return this; } }
class Derived extends Base { extra: number = 1; }
const d = new Derived();
const c = d.clone();
c.extra;
"#;
  host.insert(file.clone(), src);

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics: {diagnostics:?}"
  );

  let file_id = program.file_id(&file).expect("file id");
  let c_def = program
    .definitions_in_file(file_id)
    .into_iter()
    .find(|def| program.def_name(*def).as_deref() == Some("c"))
    .expect("definition for c");

  assert_eq!(
    program.display_type(program.type_of_def(c_def)).to_string(),
    "Derived"
  );
}

#[test]
fn explicit_this_param_works_on_member_call_but_fails_when_extracted() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    target: ScriptTarget::EsNext,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());

  let file = FileKey::new("entry.ts");
  let src = r#"
interface WithThis { f(this: this): number }
declare const o: WithThis;
const ok: number = o.f();
const g = o.f;
g();
"#;
  host.insert(file.clone(), src);

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();

  let file_id = program.file_id(&file).expect("file id");
  let g_offset = src.find("g();").expect("g call offset") as u32;

  let errors: Vec<_> = diagnostics
    .iter()
    .filter(|d| d.severity == Severity::Error)
    .collect();
  assert_eq!(errors.len(), 1, "expected one error, got {errors:?}");
  assert_eq!(
    errors[0].code.as_str(),
    codes::NO_OVERLOAD.as_str(),
    "unexpected error: {errors:?}"
  );
  assert_eq!(errors[0].primary.file, file_id, "wrong diagnostic file");
  assert!(
    errors[0].primary.range.contains(g_offset),
    "expected diagnostic on `g()`, got {errors:?}"
  );
}
