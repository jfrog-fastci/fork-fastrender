use std::fs;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use tempfile::tempdir;
use test262_semantic::executor::{default_executor, ExecError, ExecPhase};
use test262_semantic::frontmatter::Frontmatter;
use test262_semantic::report::{ExpectedOutcome, Variant};
use test262_semantic::runner::TestCase;

fn module_case(root: &tempfile::TempDir, id: &str) -> TestCase {
  let test_dir = root.path().join("test");
  TestCase {
    id: id.to_string(),
    path: test_dir.join(id),
    variant: Variant::Module,
    expected: ExpectedOutcome::Pass,
    metadata: Frontmatter::default(),
    body: String::new(),
  }
}

#[test]
fn module_missing_file_rejects_with_typeerror_reason() {
  let temp = tempdir().unwrap();
  fs::create_dir_all(temp.path().join("test")).unwrap();

  let case = module_case(&temp, "entry.js");
  let executor = default_executor();
  let cancel = Arc::new(AtomicBool::new(false));

  let err = executor
    .execute(&case, r#"import "./missing.js";"#, &cancel)
    .unwrap_err();

  let ExecError::Js(js) = err else {
    panic!("expected JS error, got {err:?}");
  };

  assert_eq!(js.phase, ExecPhase::Resolution);
  assert_eq!(js.typ.as_deref(), Some("TypeError"));
  assert!(
    !js.message.trim().is_empty() && js.message != "undefined",
    "expected non-empty rejection message, got: {:?}",
    js.message
  );
  assert!(
    js.message.contains("missing.js"),
    "expected helpful message, got: {:?}",
    js.message
  );
}

#[test]
fn module_non_utf8_file_rejects_with_typeerror_reason() {
  let temp = tempdir().unwrap();
  let test_dir = temp.path().join("test");
  fs::create_dir_all(&test_dir).unwrap();

  // Write an invalid UTF-8 file that should fail source decoding.
  fs::write(test_dir.join("bad_utf8.js"), [0xff, 0xfe]).unwrap();

  let case = module_case(&temp, "entry.js");
  let executor = default_executor();
  let cancel = Arc::new(AtomicBool::new(false));

  let err = executor
    .execute(&case, r#"import "./bad_utf8.js";"#, &cancel)
    .unwrap_err();

  let ExecError::Js(js) = err else {
    panic!("expected JS error, got {err:?}");
  };

  assert_eq!(js.phase, ExecPhase::Resolution);
  assert_eq!(js.typ.as_deref(), Some("TypeError"));
  assert!(
    !js.message.trim().is_empty() && js.message != "undefined",
    "expected non-empty rejection message, got: {:?}",
    js.message
  );
  assert!(
    js.message.to_lowercase().contains("utf-8"),
    "expected helpful message, got: {:?}",
    js.message
  );
}

#[test]
fn module_unsupported_import_attribute_rejects_with_syntaxerror_reason() {
  let temp = tempdir().unwrap();
  let test_dir = temp.path().join("test");
  fs::create_dir_all(&test_dir).unwrap();
  fs::write(test_dir.join("dep.js"), "export const x = 1;\n").unwrap();

  let case = module_case(&temp, "entry.js");
  let executor = default_executor();
  let cancel = Arc::new(AtomicBool::new(false));

  let err = executor
    .execute(&case, r#"import "./dep.js" with { foo: "bar" };"#, &cancel)
    .unwrap_err();

  let ExecError::Js(js) = err else {
    panic!("expected JS error, got {err:?}");
  };

  assert_eq!(js.phase, ExecPhase::Resolution);
  assert_eq!(js.typ.as_deref(), Some("SyntaxError"));
  assert!(
    !js.message.trim().is_empty() && js.message != "undefined",
    "expected non-empty rejection message, got: {:?}",
    js.message
  );
  assert!(
    js.message.contains("Unsupported import attribute"),
    "expected helpful message, got: {:?}",
    js.message
  );
}

#[test]
fn module_unsupported_module_type_rejects_with_syntaxerror_reason() {
  let temp = tempdir().unwrap();
  let test_dir = temp.path().join("test");
  fs::create_dir_all(&test_dir).unwrap();
  fs::write(test_dir.join("dep.js"), "export const x = 1;\n").unwrap();

  let case = module_case(&temp, "entry.js");
  let executor = default_executor();
  let cancel = Arc::new(AtomicBool::new(false));

  let err = executor
    .execute(&case, r#"import "./dep.js" with { type: "css" };"#, &cancel)
    .unwrap_err();

  let ExecError::Js(js) = err else {
    panic!("expected JS error, got {err:?}");
  };

  assert_eq!(js.phase, ExecPhase::Resolution);
  assert_eq!(js.typ.as_deref(), Some("SyntaxError"));
  assert!(
    !js.message.trim().is_empty() && js.message != "undefined",
    "expected non-empty rejection message, got: {:?}",
    js.message
  );
  assert!(
    js.message.contains("Unsupported module type"),
    "expected helpful message, got: {:?}",
    js.message
  );
}

#[test]
fn module_empty_specifier_rejects_with_typeerror_reason() {
  let temp = tempdir().unwrap();
  fs::create_dir_all(temp.path().join("test")).unwrap();

  let case = module_case(&temp, "entry.js");
  let executor = default_executor();
  let cancel = Arc::new(AtomicBool::new(false));

  let err = executor.execute(&case, r#"import "";"#, &cancel).unwrap_err();

  let ExecError::Js(js) = err else {
    panic!("expected JS error, got {err:?}");
  };

  assert_eq!(js.phase, ExecPhase::Resolution);
  assert_eq!(js.typ.as_deref(), Some("TypeError"));
  assert!(
    !js.message.trim().is_empty() && js.message != "undefined",
    "expected non-empty rejection message, got: {:?}",
    js.message
  );
  assert!(
    js.message.contains("empty"),
    "expected helpful message, got: {:?}",
    js.message
  );
}

#[test]
fn module_nul_specifier_rejects_with_typeerror_reason() {
  let temp = tempdir().unwrap();
  fs::create_dir_all(temp.path().join("test")).unwrap();

  let case = module_case(&temp, "entry.js");
  let executor = default_executor();
  let cancel = Arc::new(AtomicBool::new(false));

  // `\\0` is a NUL code point in JS string literals.
  let err = executor
    .execute(&case, r#"import "\0.js";"#, &cancel)
    .unwrap_err();

  let ExecError::Js(js) = err else {
    panic!("expected JS error, got {err:?}");
  };

  assert_eq!(js.phase, ExecPhase::Resolution);
  assert_eq!(js.typ.as_deref(), Some("TypeError"));
  assert!(
    !js.message.trim().is_empty() && js.message != "undefined",
    "expected non-empty rejection message, got: {:?}",
    js.message
  );
  assert!(
    js.message.to_lowercase().contains("nul"),
    "expected helpful message, got: {:?}",
    js.message
  );
}

#[test]
fn module_absolute_path_specifier_rejects_with_typeerror_reason() {
  let temp = tempdir().unwrap();
  fs::create_dir_all(temp.path().join("test")).unwrap();

  let case = module_case(&temp, "entry.js");
  let executor = default_executor();
  let cancel = Arc::new(AtomicBool::new(false));

  let err = executor
    .execute(&case, r#"import "/abs.js";"#, &cancel)
    .unwrap_err();

  let ExecError::Js(js) = err else {
    panic!("expected JS error, got {err:?}");
  };

  assert_eq!(js.phase, ExecPhase::Resolution);
  assert_eq!(js.typ.as_deref(), Some("TypeError"));
  assert!(
    !js.message.trim().is_empty() && js.message != "undefined",
    "expected non-empty rejection message, got: {:?}",
    js.message
  );
  assert!(
    js.message.contains("absolute"),
    "expected helpful message, got: {:?}",
    js.message
  );
}

#[cfg(unix)]
#[test]
fn module_symlink_sandbox_escape_rejects_with_typeerror_reason() {
  use std::os::unix::fs::symlink;

  let temp = tempdir().unwrap();
  let test_dir = temp.path().join("test");
  fs::create_dir_all(&test_dir).unwrap();

  // Create a file outside the sandbox root and a symlink inside the sandbox pointing to it.
  let outside = temp.path().join("outside.js");
  fs::write(&outside, "export const SHOULD_NOT_LOAD = true;\n").unwrap();
  symlink(&outside, test_dir.join("link.js")).unwrap();

  let case = module_case(&temp, "entry.js");
  let executor = default_executor();
  let cancel = Arc::new(AtomicBool::new(false));

  let err = executor
    .execute(&case, r#"import "./link.js";"#, &cancel)
    .unwrap_err();

  let ExecError::Js(js) = err else {
    panic!("expected JS error, got {err:?}");
  };

  assert_eq!(js.phase, ExecPhase::Resolution);
  assert_eq!(js.typ.as_deref(), Some("TypeError"));
  assert!(
    !js.message.trim().is_empty() && js.message != "undefined",
    "expected non-empty rejection message, got: {:?}",
    js.message
  );
  assert!(
    js.message.contains("sandbox") || js.message.contains("escapes"),
    "expected helpful message, got: {:?}",
    js.message
  );
}
