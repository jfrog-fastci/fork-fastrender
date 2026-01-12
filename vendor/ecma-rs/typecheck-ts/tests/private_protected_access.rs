use std::sync::Arc;

use diagnostics::{Severity, TextRange};
use typecheck_ts::codes;
use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{FileKey, MemoryHost, Program};

mod common;

#[test]
fn private_access_errors_outside_class() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());

  let source = r#"class C { private x: number = 1; }
const y = new C().x;
"#;
  let file = FileKey::new("entry.ts");
  host.insert(file.clone(), Arc::from(source.to_string()));

  let program = Program::new(host, vec![file]);
  let diagnostics = program.check();
  assert!(
    diagnostics
      .iter()
      .any(|d| d.severity == Severity::Error && d.code.as_str() == codes::PRIVATE_MEMBER_ACCESS.as_str()),
    "diagnostics: {:?}",
    diagnostics
  );

  let diag = diagnostics
    .iter()
    .find(|d| d.code.as_str() == codes::PRIVATE_MEMBER_ACCESS.as_str())
    .expect("TS2341 diagnostic");
  let offset = source.find(".x").expect("property access") as u32 + 1;
  assert_eq!(diag.primary.range, TextRange::new(offset, offset + 1));
}

#[test]
fn private_access_allowed_within_class() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());

  let source = r#"class C { private x: number = 1; get() { return this.x; } }
const y = new C().get();
"#;
  let file = FileKey::new("entry.ts");
  host.insert(file.clone(), Arc::from(source.to_string()));

  let program = Program::new(host, vec![file]);
  let diagnostics = program.check();
  assert!(
    diagnostics.iter().all(|d| d.severity != Severity::Error),
    "diagnostics: {:?}",
    diagnostics
  );
}

#[test]
fn protected_access_allowed_in_subclass() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());

  let source = r#"class Base { protected x: number = 1; }
class Derived extends Base { get() { return this.x; } }
"#;
  let file = FileKey::new("entry.ts");
  host.insert(file.clone(), Arc::from(source.to_string()));

  let program = Program::new(host, vec![file]);
  let diagnostics = program.check();
  assert!(
    diagnostics.iter().all(|d| d.severity != Severity::Error),
    "diagnostics: {:?}",
    diagnostics
  );
}

#[test]
fn protected_access_errors_outside_class() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());

  let source = r#"class Base { protected x: number = 1; }
const y = new Base().x;
"#;
  let file = FileKey::new("entry.ts");
  host.insert(file.clone(), Arc::from(source.to_string()));

  let program = Program::new(host, vec![file]);
  let diagnostics = program.check();
  assert!(
    diagnostics
      .iter()
      .any(|d| d.severity == Severity::Error && d.code.as_str() == codes::PROTECTED_MEMBER_ACCESS.as_str()),
    "diagnostics: {:?}",
    diagnostics
  );

  let diag = diagnostics
    .iter()
    .find(|d| d.code.as_str() == codes::PROTECTED_MEMBER_ACCESS.as_str())
    .expect("TS2445 diagnostic");
  let offset = source.find(".x").expect("property access") as u32 + 1;
  assert_eq!(diag.primary.range, TextRange::new(offset, offset + 1));
}

#[test]
fn union_missing_property_reports_ts2339_not_ts2341() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());

  let source = r#"class A { private x: number = 1; }
class B { y: number = 2; }
declare const u: A | B;
const v = u.x;
"#;
  let file = FileKey::new("entry.ts");
  host.insert(file.clone(), Arc::from(source.to_string()));

  let program = Program::new(host, vec![file]);
  let diagnostics = program.check();
  assert!(
    diagnostics
      .iter()
      .any(|d| d.severity == Severity::Error && d.code.as_str() == codes::PROPERTY_DOES_NOT_EXIST.as_str()),
    "diagnostics: {:?}",
    diagnostics
  );
  assert!(
    diagnostics
      .iter()
      .all(|d| d.code.as_str() != codes::PRIVATE_MEMBER_ACCESS.as_str()),
    "unexpected TS2341 diagnostics: {:?}",
    diagnostics
  );
}

#[test]
fn private_access_errors_through_type_param_constraint() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());

  let source = r#"class C { private x: number = 1; }
function f<T extends C>(t: T) { return t.x; }
"#;
  let file = FileKey::new("entry.ts");
  host.insert(file.clone(), Arc::from(source.to_string()));

  let program = Program::new(host, vec![file]);
  let diagnostics = program.check();
  let diag = diagnostics
    .iter()
    .find(|d| d.severity == Severity::Error && d.code.as_str() == codes::PRIVATE_MEMBER_ACCESS.as_str())
    .expect("TS2341 diagnostic");
  let offset = source.find(".x").expect("property access") as u32 + 1;
  assert_eq!(diag.primary.range, TextRange::new(offset, offset + 1));
}

#[test]
fn protected_access_errors_through_type_param_constraint() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());

  let source = r#"class Base { protected x: number = 1; }
function f<T extends Base>(t: T) { return t.x; }
"#;
  let file = FileKey::new("entry.ts");
  host.insert(file.clone(), Arc::from(source.to_string()));

  let program = Program::new(host, vec![file]);
  let diagnostics = program.check();
  let diag = diagnostics
    .iter()
    .find(|d| {
      d.severity == Severity::Error && d.code.as_str() == codes::PROTECTED_MEMBER_ACCESS.as_str()
    })
    .expect("TS2445 diagnostic");
  let offset = source.find(".x").expect("property access") as u32 + 1;
  assert_eq!(diag.primary.range, TextRange::new(offset, offset + 1));
}

#[test]
fn private_access_errors_on_union_with_private_member() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());

  let source = r#"class A { private x: number = 1; }
class B { x: number = 2; }
declare const u: A | B;
const v = u.x;
"#;
  let file = FileKey::new("entry.ts");
  host.insert(file.clone(), Arc::from(source.to_string()));

  let program = Program::new(host, vec![file]);
  let diagnostics = program.check();
  let diag = diagnostics
    .iter()
    .find(|d| d.severity == Severity::Error && d.code.as_str() == codes::PRIVATE_MEMBER_ACCESS.as_str())
    .expect("TS2341 diagnostic");
  let offset = source.find(".x").expect("property access") as u32 + 1;
  assert_eq!(diag.primary.range, TextRange::new(offset, offset + 1));
}
