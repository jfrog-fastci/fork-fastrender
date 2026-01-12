mod common;

use typecheck_ts::codes;
use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn postfix_update_visits_operand_for_unknown_identifiers() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());

  let file = FileKey::new("main.ts");
  host.insert(file.clone(), "export function f() { missing++; }");

  let program = Program::new(host, vec![file]);
  let diagnostics = program.check();
  assert!(
    diagnostics
      .iter()
      .any(|d| d.code.as_str() == codes::UNKNOWN_IDENTIFIER.as_str()),
    "expected TC0005 (unknown identifier) diagnostic, got {diagnostics:?}"
  );
}

#[test]
fn postfix_update_triggers_property_used_before_initialization() {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());

  let file = FileKey::new("main.ts");
  host.insert(
    file.clone(),
    r#"
class C {
  x = this.y++;
  y = 1;
}
"#,
  );

  let program = Program::new(host, vec![file]);
  let diagnostics = program.check();
  assert!(
    diagnostics
      .iter()
      .any(|d| d.code.as_str() == codes::PROPERTY_USED_BEFORE_INITIALIZATION.as_str()),
    "expected TS2729 (property used before initialization) diagnostic, got {diagnostics:?}"
  );
}

