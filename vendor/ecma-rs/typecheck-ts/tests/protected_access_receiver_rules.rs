use diagnostics::TextRange;
use typecheck_ts::{FileKey, MemoryHost, Program};

fn run(source: &str) -> Vec<diagnostics::Diagnostic> {
  let mut host = MemoryHost::default();
  let key = FileKey::new("main.ts");
  host.insert(key.clone(), source);
  let program = Program::new(host, vec![key]);
  program.check()
}

#[test]
fn protected_member_access_requires_receiver_derived_from_current_class() {
  let source = "class Base { protected x: number = 1; }\n\
class Derived extends Base { f(b: Base) { return b.x; } }\n";
  let diagnostics = run(source);
  assert_eq!(
    diagnostics.len(),
    1,
    "expected one diagnostic, got {diagnostics:?}"
  );
  assert_eq!(diagnostics[0].code.as_str(), "TS2445");
  let member = source.find("b.x").expect("member access present");
  let x_start = member + 2;
  let x_end = x_start + 1;
  assert_eq!(
    diagnostics[0].primary.range,
    TextRange::new(x_start as u32, x_end as u32)
  );
}

#[test]
fn protected_member_access_allows_this_and_same_class_instances() {
  let source = "class Base { protected x: number = 1; }\n\
 class Derived extends Base {\n\
   g(d: Derived) { return d.x; }\n\
   h() { return this.x; }\n\
   i() { return super.x; }\n\
 }\n";
  let diagnostics = run(source);
  assert!(
    diagnostics.is_empty(),
    "expected no diagnostics, got {diagnostics:?}"
  );
}

#[test]
fn protected_static_member_access_requires_receiver_derived_from_current_class() {
  let source = "class Base { protected static x: number = 1; }\n\
class Derived extends Base { static f(b: typeof Base) { return b.x; } }\n";
  let diagnostics = run(source);
  assert_eq!(
    diagnostics.len(),
    1,
    "expected one diagnostic, got {diagnostics:?}"
  );
  assert_eq!(diagnostics[0].code.as_str(), "TS2445");
  let member = source.find("b.x").expect("member access present");
  let x_start = member + 2;
  let x_end = x_start + 1;
  assert_eq!(
    diagnostics[0].primary.range,
    TextRange::new(x_start as u32, x_end as u32)
  );
}

#[test]
fn protected_static_member_access_allows_this_super_and_current_class_constructors() {
  let source = "class Base { protected static x: number = 1; }\n\
class Derived extends Base {\n\
  static g(d: typeof Derived) { return d.x; }\n\
  static h() { return this.x; }\n\
  static i() { return super.x; }\n\
  f() { return Derived.x; }\n\
}\n";
  let diagnostics = run(source);
  assert!(
    diagnostics.is_empty(),
    "expected no diagnostics, got {diagnostics:?}"
  );
}
