mod common;

use std::sync::Arc;

use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{codes, FileKey, MemoryHost, Program};

#[test]
fn super_member_call_uses_derived_this_instance() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());

  let file = FileKey::new("a.ts");
  let src = r#"
class Base {
  foo(this: Sub): number { return 1; }
}
class Sub extends Base {
  bar() { return super.foo(); }
}
"#;
  host.insert(file.clone(), Arc::from(src));
  let program = Program::new(host, vec![file.clone()]);

  let diagnostics = program.check();
  assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");

  let file_id = program.file_id(&file).expect("file id");
  let offset =
    src.find("super.foo()").expect("call offset") as u32 + "super.foo".len() as u32;
  let ty = program.type_at(file_id, offset).expect("type_at super.foo()");
  assert_eq!(program.display_type(ty).to_string(), "number");
}

#[test]
fn super_member_call_uses_derived_this_instance_computed() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());

  let file = FileKey::new("a.ts");
  let src = r#"
class Base {
  foo(this: Sub): number { return 1; }
}
class Sub extends Base {
  bar() { return super["foo"](); }
}
"#;
  host.insert(file.clone(), Arc::from(src));
  let program = Program::new(host, vec![file.clone()]);

  let diagnostics = program.check();
  assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");

  let file_id = program.file_id(&file).expect("file id");
  let offset = src
    .find("super[\"foo\"]()")
    .expect("call offset") as u32
    + "super[\"foo\"]".len() as u32;
  let ty = program.type_at(file_id, offset).expect("type_at super[\"foo\"]()");
  assert_eq!(program.display_type(ty).to_string(), "number");
}

#[test]
fn super_member_call_uses_derived_this_instance_optional_chain() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());

  let file = FileKey::new("a.ts");
  let src = r#"
class Base {
  foo(this: Sub): number { return 1; }
}
class Sub extends Base {
  bar() { return super.foo?.(); }
}
"#;
  host.insert(file.clone(), Arc::from(src));
  let program = Program::new(host, vec![file.clone()]);

  let diagnostics = program.check();
  assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");

  let file_id = program.file_id(&file).expect("file id");
  let offset =
    src.find("super.foo?.()").expect("call offset") as u32 + "super.foo".len() as u32;
  let ty = program.type_at(file_id, offset).expect("type_at super.foo?.()");
  assert_eq!(program.display_type(ty).to_string(), "number");
}

#[test]
fn super_member_call_uses_derived_this_static() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());

  let file = FileKey::new("a.ts");
  let src = r#"
class Base {
  static foo(this: typeof Sub): number { return 1; }
}
class Sub extends Base {
  static bar() { return super.foo(); }
}
"#;
  host.insert(file.clone(), Arc::from(src));
  let program = Program::new(host, vec![file.clone()]);

  let diagnostics = program.check();
  assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");

  let file_id = program.file_id(&file).expect("file id");
  let offset =
    src.find("super.foo()").expect("call offset") as u32 + "super.foo".len() as u32;
  let ty = program.type_at(file_id, offset).expect("type_at super.foo()");
  assert_eq!(program.display_type(ty).to_string(), "number");
}

#[test]
fn super_member_call_rejects_incompatible_this() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());

  let file = FileKey::new("a.ts");
  let src = r#"
class Base {
  foo(this: { x: number }): number { return 1; }
}
class Sub extends Base {
  x = "no";
  bar() { return super.foo(); }
}
"#;
  host.insert(file.clone(), Arc::from(src));
  let program = Program::new(host, vec![file.clone()]);

  let diagnostics = program.check();
  assert!(
    diagnostics
      .iter()
      .any(|diag| diag.code.as_str() == codes::NO_OVERLOAD.as_str()),
    "expected NO_OVERLOAD diagnostic, got: {diagnostics:?}"
  );
}
