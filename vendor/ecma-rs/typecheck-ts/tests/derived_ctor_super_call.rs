use typecheck_ts::codes;
use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn this_before_super_is_error() {
  let source = r#"
class B { constructor() {} }
class C extends B {
  constructor() {
    const y = this;
    super();
  }
}
"#;

  let key = FileKey::new("main.ts");
  let mut host = MemoryHost::default();
  host.insert(key.clone(), source);
  let program = Program::new(host, vec![key]);
  let diagnostics = program.check();
  assert!(
    diagnostics
      .iter()
      .any(|diag| diag.code.as_str() == codes::SUPER_MUST_BE_CALLED_BEFORE_THIS.as_str()),
    "expected TS17009 in diagnostics, got {diagnostics:#?}",
  );
}

#[test]
fn missing_super_on_some_paths_is_error() {
  let source = r#"
class B { constructor() {} }
class C extends B {
  constructor(cond: boolean) {
    if (cond) {
      super();
    }
    const y = this;
  }
}
"#;

  let key = FileKey::new("main.ts");
  let mut host = MemoryHost::default();
  host.insert(key.clone(), source);
  let program = Program::new(host, vec![key]);
  let diagnostics = program.check();
  assert!(
    diagnostics
      .iter()
      .any(|diag| diag.code.as_str() == codes::SUPER_MUST_BE_CALLED_BEFORE_THIS.as_str()),
    "expected TS17009 in diagnostics, got {diagnostics:#?}",
  );
}

#[test]
fn super_called_on_all_paths_is_ok() {
  let source = r#"
class B { constructor() {} }
class C extends B {
  constructor() {
    super();
    const y = this;
  }
}
"#;

  let key = FileKey::new("main.ts");
  let mut host = MemoryHost::default();
  host.insert(key.clone(), source);
  let program = Program::new(host, vec![key]);
  let diagnostics = program.check();
  assert!(
    diagnostics
      .iter()
      .all(|diag| diag.code.as_str() != codes::SUPER_MUST_BE_CALLED_BEFORE_THIS.as_str()),
    "expected no TS17009 in diagnostics, got {diagnostics:#?}",
  );
}

#[test]
fn super_property_access_before_super_is_error() {
  let source = r#"
class B { foo() {} }
class C extends B {
  constructor() {
    super.foo();
    super();
  }
}
"#;

  let key = FileKey::new("main.ts");
  let mut host = MemoryHost::default();
  host.insert(key.clone(), source);
  let program = Program::new(host, vec![key]);
  let diagnostics = program.check();
  assert!(
    diagnostics
      .iter()
      .any(|diag| diag.code.as_str() == codes::SUPER_MUST_BE_CALLED_BEFORE_THIS.as_str()),
    "expected TS17009 in diagnostics, got {diagnostics:#?}",
  );
}

