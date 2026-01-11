use std::collections::HashMap;
use std::sync::Arc;

use diagnostics::Diagnostic;
use native_js::strict;
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

fn validate(source: &str, kind: FileKind) -> Vec<Diagnostic> {
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

  let file = program.file_id(&key).expect("file id");
  strict::validate(&program, &[file])
}

fn assert_has_code(diags: &[Diagnostic], code: &str) {
  assert!(
    diags.iter().any(|d| d.code == code),
    "expected diagnostic code {code}, got: {:?}",
    diags.iter().map(|d| d.code.as_str()).collect::<Vec<_>>()
  );
}

#[test]
fn rejects_any_in_expression_types() {
  let diags = validate(
    r#"
      JSON.parse("1");
    "#,
    FileKind::Ts,
  );
  assert_has_code(&diags, "NJS0001");
}

#[test]
fn rejects_any_in_type_alias() {
  let diags = validate(
    r#"
      type T = any;
      export const x = 1;
      void x;
    "#,
    FileKind::Ts,
  );
  assert_has_code(&diags, "NJS0001");
}

#[test]
fn rejects_any_in_unused_return_annotation() {
  let diags = validate(
    r#"
      function f(): any {
        return 1;
      }
      export const x = 1;
      void x;
    "#,
    FileKind::Ts,
  );
  assert_has_code(&diags, "NJS0001");
}

#[test]
fn rejects_any_nested_in_object_types() {
  let diags = validate(
    r#"
      type T = { x: Function };
      const t: T = { x() {} };
      void t;
    "#,
    FileKind::Ts,
  );
  assert_has_code(&diags, "NJS0001");
}

#[test]
fn rejects_any_in_exported_function_signature() {
  let diags = validate(
    r#"
      export declare function f(): Function;
    "#,
    FileKind::Ts,
  );
  assert_has_code(&diags, "NJS0001");
}

#[test]
fn rejects_any_in_pattern_types() {
  let source = r#"const onlyAny = JSON.parse("1");"#;
  let diags = validate(source, FileKind::Ts);
  assert_has_code(&diags, "NJS0001");

  let start = source.find("onlyAny").expect("needle") as u32;
  let end = start + "onlyAny".len() as u32;
  assert!(
    diags
      .iter()
      .any(|d| d.code == "NJS0001" && d.primary.range.start == start && d.primary.range.end == end),
    "expected NJS0001 span for pattern `onlyAny` ({}..{}), got: {diags:#?}",
    start,
    end
  );
}

#[test]
fn rejects_type_assertions() {
  let diags = validate(
    r#"
      const x = 1 as number;
      void x;
    "#,
    FileKind::Ts,
  );
  assert_has_code(&diags, "NJS0002");
}

#[test]
fn rejects_non_null_assertions() {
  let diags = validate(
    r#"
      function f(x?: string) {
        return x!;
      }
      f("ok");
    "#,
    FileKind::Ts,
  );
  assert_has_code(&diags, "NJS0003");
}

#[test]
fn rejects_eval() {
  let diags = validate(
    r#"
      eval("1 + 1");
    "#,
    FileKind::Ts,
  );
  assert_has_code(&diags, "NJS0004");
}

#[test]
fn rejects_eval_call() {
  let diags = validate(
    r#"
      eval.call(null, "1 + 1");
    "#,
    FileKind::Ts,
  );
  assert_has_code(&diags, "NJS0004");
}

#[test]
fn rejects_eval_apply() {
  let diags = validate(
    r#"
      eval.apply(null, ["1 + 1"]);
    "#,
    FileKind::Ts,
  );
  assert_has_code(&diags, "NJS0004");
}

#[test]
fn rejects_eval_bind() {
  let diags = validate(
    r#"
      eval.bind(null, "1 + 1");
    "#,
    FileKind::Ts,
  );
  assert_has_code(&diags, "NJS0004");
}

#[test]
fn rejects_indirect_eval() {
  let diags = validate(
    r#"
      (eval, eval)("1 + 1");
    "#,
    FileKind::Ts,
  );
  assert_has_code(&diags, "NJS0004");
}

#[test]
fn rejects_member_eval() {
  let diags = validate(
    r#"
      const obj = { eval };
      obj.eval("1 + 1");
    "#,
    FileKind::Ts,
  );
  assert_has_code(&diags, "NJS0004");
}

#[test]
fn rejects_computed_member_eval() {
  let diags = validate(
    r#"
      const obj = { eval };
      obj["eval"]("1 + 1");
    "#,
    FileKind::Ts,
  );
  assert_has_code(&diags, "NJS0004");
}

#[test]
fn rejects_new_function() {
  let diags = validate(
    r#"
      const f = new Function("return 1;");
      void f;
    "#,
    FileKind::Ts,
  );
  assert_has_code(&diags, "NJS0005");
}

#[test]
fn rejects_function_call() {
  let diags = validate(
    r#"
      Function.call(null, "return 1;");
    "#,
    FileKind::Ts,
  );
  assert_has_code(&diags, "NJS0005");
}

#[test]
fn rejects_function_apply() {
  let diags = validate(
    r#"
      Function.apply(null, ["return 1;"]);
    "#,
    FileKind::Ts,
  );
  assert_has_code(&diags, "NJS0005");
}

#[test]
fn rejects_function_bind() {
  let diags = validate(
    r#"
      Function.bind(null, "return 1;");
    "#,
    FileKind::Ts,
  );
  assert_has_code(&diags, "NJS0005");
}

#[test]
fn rejects_indirect_function() {
  let diags = validate(
    r#"
      new (Function, Function)("return 1;");
    "#,
    FileKind::Ts,
  );
  assert_has_code(&diags, "NJS0005");
}

#[test]
fn rejects_member_new_function() {
  let diags = validate(
    r#"
      const obj = { Function };
      new obj.Function("return 1;");
    "#,
    FileKind::Ts,
  );
  assert_has_code(&diags, "NJS0005");
}

#[test]
fn rejects_computed_member_new_function() {
  let diags = validate(
    r#"
      const obj = { Function };
      new obj["Function"]("return 1;");
    "#,
    FileKind::Ts,
  );
  assert_has_code(&diags, "NJS0005");
}

#[test]
fn rejects_with_statement() {
  let diags = validate(
    r#"
      const obj = { x: 1 };
      with (obj) {
      }
    "#,
    FileKind::Js,
  );
  assert_has_code(&diags, "NJS0006");
}

#[test]
fn rejects_dynamic_member_access() {
  let diags = validate(
    r#"
      const obj: Record<string, number> = { a: 1 };
      const key = "a";
      obj[key];
    "#,
    FileKind::Ts,
  );
  assert_has_code(&diags, "NJS0007");
}

#[test]
fn allows_computed_member_access_with_literal_key() {
  let diags = validate(
    r#"
      const obj = { a: 1 };
      const x = obj["a"];
      const y = [1, 2][0];
      void x;
      void y;
    "#,
    FileKind::Ts,
  );
  assert!(
    diags.is_empty(),
    "expected no strict diagnostics, got: {diags:#?}"
  );
}

#[test]
fn rejects_arguments_identifier() {
  let diags = validate(
    r#"
      function f() {
        const arguments: number[] = [1];
        return arguments[0];
      }
      f();
    "#,
    FileKind::Ts,
  );
  assert_has_code(&diags, "NJS0008");
}
