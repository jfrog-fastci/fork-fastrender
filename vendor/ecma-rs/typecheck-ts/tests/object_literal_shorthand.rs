mod common;

use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{codes, FileKey, MemoryHost, Program};

fn test_host() -> MemoryHost {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  host
}

#[test]
fn missing_shorthand_emits_unknown_identifier_and_does_not_cascade() {
  let mut host = test_host();
  let file = FileKey::new("main.ts");
  host.insert(
    file.clone(),
    r#"
export const o = { missing };
export const ok: number = o.missing;
"#,
  );

  let program = Program::new(host, vec![file]);
  let diagnostics = program.check();

  assert!(
    diagnostics
      .iter()
      .any(|diag| diag.code.as_str() == codes::UNKNOWN_IDENTIFIER.as_str()),
    "expected UNKNOWN_IDENTIFIER, got {diagnostics:?}"
  );
  assert!(
    diagnostics
      .iter()
      .all(|diag| diag.code.as_str() != codes::TYPE_MISMATCH.as_str()),
    "missing shorthand should use TS-style error type (`any`) to avoid cascades; got {diagnostics:?}"
  );
}

#[test]
fn shorthand_records_expr_type_for_type_at() {
  let mut host = test_host();
  let file = FileKey::new("main.ts");
  let source = r#"
const a = 1;
export const o = { a };
"#;
  host.insert(file.clone(), source);

  let program = Program::new(host, vec![file.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "unexpected diagnostics: {diagnostics:?}"
  );

  let file_id = program.file_id(&file).expect("file id");
  let shorthand_offset = source
    .find("{ a }")
    .map(|idx| idx as u32 + "{ ".len() as u32)
    .expect("offset of shorthand `a`");

  let ty = program
    .type_at(file_id, shorthand_offset)
    .expect("type at shorthand `a`");
  assert_ne!(
    program.display_type(ty).to_string(),
    "unknown",
    "shorthand identifier should have a recorded expression type"
  );
}

#[test]
fn shorthand_property_value_is_widened_like_other_prop_initializers() {
  let mut host = test_host();
  let file = FileKey::new("main.ts");
  host.insert(
    file.clone(),
    r#"
const a = 1;
export const o = { a };
export const ok: number = o.a;
export const bad: 1 = o.a;
"#,
  );

  let program = Program::new(host, vec![file]);
  let diagnostics = program.check();

  assert!(
    diagnostics
      .iter()
      .any(|diag| diag.code.as_str() == codes::TYPE_MISMATCH.as_str()),
    "expected TYPE_MISMATCH due to widened shorthand property, got {diagnostics:?}"
  );
}

