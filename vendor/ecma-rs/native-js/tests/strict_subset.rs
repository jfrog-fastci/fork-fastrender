use std::collections::HashMap;
use std::sync::Arc;

use diagnostics::Diagnostic;
use native_js::validate::validate_strict_subset;
use typecheck_ts::lib_support::FileKind;
use typecheck_ts::{FileKey, Host, HostError, Program};

#[derive(Clone, Default)]
struct TestHost {
  files: HashMap<FileKey, Arc<str>>,
  kinds: HashMap<FileKey, FileKind>,
}

impl TestHost {
  fn insert(&mut self, key: FileKey, kind: FileKind, source: &str) {
    self.files.insert(key.clone(), Arc::from(source.to_string()));
    self.kinds.insert(key, kind);
  }
}

impl Host for TestHost {
  fn file_text(&self, file: &FileKey) -> Result<Arc<str>, HostError> {
    self
      .files
      .get(file)
      .cloned()
      .ok_or_else(|| HostError::new(format!("missing file {file:?}")))
  }

  fn resolve(&self, _from: &FileKey, _specifier: &str) -> Option<FileKey> {
    None
  }

  fn file_kind(&self, file: &FileKey) -> FileKind {
    self.kinds.get(file).copied().unwrap_or(FileKind::Ts)
  }
}

fn validate(source: &str, kind: FileKind) -> Result<(), Vec<Diagnostic>> {
  let mut host = TestHost::default();
  let key = match kind {
    FileKind::Js => FileKey::new("main.js"),
    FileKind::Jsx => FileKey::new("main.jsx"),
    FileKind::Ts => FileKey::new("main.ts"),
    FileKind::Tsx => FileKey::new("main.tsx"),
    FileKind::Dts => FileKey::new("main.d.ts"),
  };
  host.insert(key.clone(), kind, source);

  let program = Program::new(host, vec![key.clone()]);
  let tc_diags = program.check();
  assert!(
    tc_diags.is_empty(),
    "expected sample to typecheck cleanly, got: {tc_diags:#?}"
  );

  validate_strict_subset(&program)
}

fn assert_has_code(diags: &[Diagnostic], code: &str) {
  assert!(
    diags.iter().any(|d| d.code == code),
    "expected diagnostic code {code}, got: {:?}",
    diags.iter().map(|d| d.code.as_str()).collect::<Vec<_>>()
  );
}

#[test]
fn rejects_explicit_any() {
  let err = validate("let x: any = 1;\nvoid x;\n", FileKind::Ts).unwrap_err();
  assert_has_code(&err, "NJS0010");
}

#[test]
fn rejects_inferred_any() {
  let err = validate("const x = JSON.parse(\"1\");\nvoid x;\n", FileKind::Ts).unwrap_err();
  assert_has_code(&err, "NJS0010");
}

#[test]
fn rejects_union_type() {
  let err = validate("const x: number | string = 1;\nvoid x;\n", FileKind::Ts).unwrap_err();
  assert_has_code(&err, "NJS0010");
}

#[test]
fn rejects_with_statement() {
  let err = validate("with (Math) {}\n", FileKind::Js).unwrap_err();
  assert_has_code(&err, "NJS0009");
}

#[test]
fn rejects_object_literal() {
  let err = validate("const obj = { x: 1 };\nvoid obj;\n", FileKind::Ts).unwrap_err();
  assert_has_code(&err, "NJS0009");
}

#[test]
fn rejects_array_literal() {
  let err = validate("const xs = [1, 2];\nvoid xs;\n", FileKind::Ts).unwrap_err();
  assert_has_code(&err, "NJS0009");
}

#[test]
fn rejects_member_access() {
  let err = validate("const x = Math.PI;\nvoid x;\n", FileKind::Ts).unwrap_err();
  assert_has_code(&err, "NJS0009");
}

#[test]
fn rejects_eval() {
  let err = validate("eval(\"1 + 1\");\n", FileKind::Ts).unwrap_err();
  assert_has_code(&err, "NJS0009");
}

#[test]
fn rejects_new_function() {
  let err = validate("new Function(\"return 1;\");\n", FileKind::Ts).unwrap_err();
  assert_has_code(&err, "NJS0009");
}

#[test]
fn rejects_arguments_object() {
  let err = validate(
    r#"
      function f() {
        void arguments;
      }
      f();
    "#,
    FileKind::Ts,
  )
  .unwrap_err();
  assert_has_code(&err, "NJS0009");
}

#[test]
fn rejects_try_and_throw() {
  let err = validate(
    r#"
      try {
        throw 1;
      } catch (e) {
      }
    "#,
    FileKind::Ts,
  )
  .unwrap_err();
  assert_has_code(&err, "NJS0009");
}

#[test]
fn accepts_basic_numeric_function() {
  let ok = validate(
    r#"
      function add(a: number, b: number): number {
        return a + b * 2;
      }
    "#,
    FileKind::Ts,
  );
  assert!(ok.is_ok(), "expected strict-subset validation to pass, got: {ok:#?}");
}
