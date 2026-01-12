use typecheck_ts::codes;
use typecheck_ts::{FileKey, MemoryHost, Program};

#[test]
fn unknown_identifier_does_not_cascade_into_type_mismatch() {
  let mut host = MemoryHost::new();
  let key = FileKey::new("main.ts");
  host.insert(key.clone(), "const n: number = value;");

  let program = Program::new(host, vec![key]);
  let diagnostics = program.check();

  assert!(
    diagnostics
      .iter()
      .any(|diag| diag.code.as_str() == codes::UNKNOWN_IDENTIFIER.as_str()),
    "expected unknown identifier diagnostic, got {diagnostics:?}"
  );
  assert!(
    !diagnostics
      .iter()
      .any(|diag| diag.code.as_str() == codes::TYPE_MISMATCH.as_str()),
    "expected missing identifier to not cascade into TS2322, got {diagnostics:?}"
  );
}

#[test]
fn unresolved_module_import_does_not_cascade_into_type_mismatch() {
  let mut host = MemoryHost::new();
  let key = FileKey::new("main.ts");
  host.insert(
    key.clone(),
    "import { foo } from \"./missing\";\nconst n: number = foo;",
  );

  let program = Program::new(host, vec![key.clone()]);
  let file_id = program.file_id(&key).expect("main.ts should be loaded");
  let diagnostics = program.check();

  assert!(
    diagnostics.iter().any(|diag| diag.primary.file == file_id
      && diag.code.as_str() == codes::UNRESOLVED_MODULE.as_str()),
    "expected unresolved module diagnostic, got {diagnostics:?}"
  );
  assert!(
    !diagnostics
      .iter()
      .any(|diag| diag.code.as_str() == codes::UNKNOWN_IDENTIFIER.as_str()),
    "expected missing module import to not cascade into TC0005, got {diagnostics:?}"
  );
  assert!(
    !diagnostics
      .iter()
      .any(|diag| diag.code.as_str() == codes::TYPE_MISMATCH.as_str()),
    "expected missing module import to not cascade into TS2322, got {diagnostics:?}"
  );
}
