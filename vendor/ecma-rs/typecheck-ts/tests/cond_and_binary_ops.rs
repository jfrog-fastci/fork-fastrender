mod common;

use typecheck_ts::codes;
use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{FileKey, MemoryHost, Program};

fn test_host() -> MemoryHost {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  host
}

#[test]
fn conditional_operator_visits_test_expression() {
  let mut host = test_host();
  let key = FileKey::new("main.ts");
  host.insert(key.clone(), "export const x = missing ? 1 : 2;\n");

  let program = Program::new(host, vec![key]);
  let diagnostics = program.check();

  assert!(
    diagnostics
      .iter()
      .any(|diag| diag.code.as_str() == codes::UNKNOWN_IDENTIFIER.as_str()),
    "expected {}, got {diagnostics:?}",
    codes::UNKNOWN_IDENTIFIER.as_str()
  );
}

#[test]
fn comma_operator_has_rhs_type() {
  let mut host = test_host();
  let key = FileKey::new("main.ts");
  host.insert(
    key.clone(),
    "declare function takesNumber(x: number): void;\n\
     export function f() {\n\
       takesNumber((\"a\", 1));\n\
     }\n",
  );

  let program = Program::new(host, vec![key]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics: {diagnostics:?}"
  );
}

#[test]
fn in_operator_returns_boolean() {
  let mut host = test_host();
  let key = FileKey::new("main.ts");
  host.insert(key.clone(), "export const ok: boolean = \"x\" in { x: 1 };\n");

  let program = Program::new(host, vec![key]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics: {diagnostics:?}"
  );
}

#[test]
fn instanceof_operator_returns_boolean() {
  let mut host = test_host();
  let key = FileKey::new("main.ts");
  host.insert(key.clone(), "export const ok: boolean = ({}) instanceof Object;\n");

  let program = Program::new(host, vec![key]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics: {diagnostics:?}"
  );
}

