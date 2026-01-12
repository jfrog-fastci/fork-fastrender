use std::sync::Arc;

mod common;

use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{codes, FileKey, MemoryHost, Program};

#[test]
fn super_call_with_spread_array_literal_is_ok() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  let file = FileKey::new("main.ts");
  let source = r#"
class Base { constructor(x: number, y: string) {} }
class Derived extends Base { constructor() { super(...[1, "ok"]); } }
"#;
  host.insert(file.clone(), Arc::from(source));

  let program = Program::new(host, vec![file]);
  let diagnostics = program.check();
  assert!(diagnostics.is_empty(), "diagnostics: {diagnostics:?}");
}

#[test]
fn super_call_with_spread_array_literal_reports_arg_mismatch() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  let file = FileKey::new("main.ts");
  let source = r#"
class Base { constructor(x: number, y: string) {} }
class Derived extends Base { constructor() { super(...["bad", "ok"]); } }
"#;
  host.insert(file.clone(), Arc::from(source));

  let program = Program::new(host, vec![file]);
  let diagnostics = program.check();

  assert!(
    diagnostics
      .iter()
      .any(|diag| diag.code.as_str() == codes::NO_OVERLOAD.as_str()),
    "expected NO_OVERLOAD diagnostic, got {diagnostics:?}"
  );
}
