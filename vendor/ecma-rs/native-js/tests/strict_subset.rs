use std::collections::HashMap;
use std::sync::Arc;

use diagnostics::Diagnostic;
use native_js::validate::validate_strict_subset;
use typecheck_ts::lib_support::FileKind;
use typecheck_ts::lib_support::LibFile;
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

  fn lib_files(&self) -> Vec<LibFile> {
    // Match the `native-js` binary: declare a tiny builtin surface that can be
    // codegen'd by the current backend without introducing `any`.
    vec![LibFile {
      key: FileKey::new("native-js:test-builtins.d.ts"),
      name: Arc::from("native-js test builtins"),
      kind: FileKind::Dts,
      text: Arc::from("declare function print(value: number): void;\n"),
    }]
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
  let err = validate("let x: any = 1;\nx;\n", FileKind::Ts).unwrap_err();
  assert_has_code(&err, "NJS0010");
}

#[test]
fn rejects_inferred_any() {
  let err = validate("const x = JSON.parse(\"1\");\nx;\n", FileKind::Ts).unwrap_err();
  assert_has_code(&err, "NJS0010");
}

#[test]
fn rejects_union_type() {
  let err = validate("const x: number | string = 1;\nx;\n", FileKind::Ts).unwrap_err();
  assert_has_code(&err, "NJS0010");
}

#[test]
fn rejects_with_statement() {
  let err = validate("with (Math) {}\n", FileKind::Js).unwrap_err();
  assert_has_code(&err, "NJS0009");
}

#[test]
fn rejects_object_literal() {
  let err = validate("const obj = { x: 1 };\nobj;\n", FileKind::Ts).unwrap_err();
  assert_has_code(&err, "NJS0009");
}

#[test]
fn rejects_string_literal() {
  let err = validate("const s = \"hi\";\ns;\n", FileKind::Ts).unwrap_err();
  assert_has_code(&err, "NJS0009");
}

#[test]
fn rejects_var_decl_without_initializer() {
  let err = validate("let x: number;\nx = 1;\nx;\n", FileKind::Ts).unwrap_err();
  assert_has_code(&err, "NJS0009");
}

#[test]
fn rejects_switch_statement() {
  let err = validate("switch (1) { case 1: break; }\n", FileKind::Js).unwrap_err();
  assert_has_code(&err, "NJS0009");
}

#[test]
fn rejects_for_in_loop() {
  let err = validate("const obj: any = 0 as any;\nfor (const k in obj) { k; }\n", FileKind::Ts).unwrap_err();
  assert_has_code(&err, "NJS0009");
}

#[test]
fn rejects_non_i32_numeric_literals() {
  let err = validate("const a: number = 1.5;\nconst b: number = 1e3;\na;\nb;\n", FileKind::Ts).unwrap_err();
  assert_has_code(&err, "NJS0009");
}

#[test]
fn rejects_array_literal() {
  let err = validate("const xs = [1, 2];\nvoid xs;\n", FileKind::Ts).unwrap_err();
  assert_has_code(&err, "NJS0009");
}

#[test]
fn rejects_float_literal() {
  let err = validate("const x = 1.5;\n", FileKind::Ts).unwrap_err();
  assert_has_code(&err, "NJS0009");
}

#[test]
fn rejects_out_of_range_numeric_literal() {
  let err = validate("const x = 2147483648;\n", FileKind::Ts).unwrap_err();
  assert_has_code(&err, "NJS0009");
}

#[test]
fn rejects_string_literal() {
  let err = validate("const s = \"hi\";\nvoid s;\n", FileKind::Ts).unwrap_err();
  assert_has_code(&err, "NJS0009");
}

#[test]
fn rejects_conditional_expression() {
  let err = validate("const x = true ? 1 : 2;\nvoid x;\n", FileKind::Ts).unwrap_err();
  assert_has_code(&err, "NJS0009");
}

#[test]
fn rejects_logical_or() {
  let err = validate("const x = 1 || 2;\nvoid x;\n", FileKind::Ts).unwrap_err();
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
fn rejects_switch_statement() {
  let err = validate(
    r#"
      switch (1) {
        case 1:
          break;
        default:
          break;
      }
    "#,
    FileKind::Ts,
  )
  .unwrap_err();
  assert_has_code(&err, "NJS0009");
}

#[test]
fn rejects_var_decl_without_initializer() {
  let err = validate(
    r#"
      let x: number;
      x = 1;
      print(x);
    "#,
    FileKind::Ts,
  )
  .unwrap_err();
  assert_has_code(&err, "NJS0009");
}

#[test]
fn rejects_uninitialized_for_loop_initializer() {
  let err = validate(
    r#"
      for (let i: number; ; ) {
        i = 0;
        print(i);
        break;
      }
    "#,
    FileKind::Ts,
  )
  .unwrap_err();
  assert_has_code(&err, "NJS0009");
}

#[test]
fn accepts_print_statement() {
  let ok = validate("print(1);\n", FileKind::Ts);
  assert!(ok.is_ok(), "expected strict-subset validation to pass, got: {ok:#?}");
}

#[test]
fn rejects_print_used_as_expression() {
  let err = validate(
    r#"
      function f(): number {
        const x = print(1);
        return 0;
      }

      f();
    "#,
    FileKind::Ts,
  )
  .unwrap_err();
  assert_has_code(&err, "NJS0009");
}

#[test]
fn accepts_direct_function_call() {
  let ok = validate(
    r#"
      function helper(x: number): number {
        return x * 2;
      }

      function run(): number {
        return helper(21);
      }

      run();
    "#,
    FileKind::Ts,
  );
  assert!(ok.is_ok(), "expected strict-subset validation to pass, got: {ok:#?}");
}

#[test]
fn rejects_nested_function_call() {
  let err = validate(
    r#"
      function outer(x: number): number {
        function inner(y: number): number {
          return y + 1;
        }
        return inner(x);
      }

      outer(1);
    "#,
    FileKind::Ts,
  )
  .unwrap_err();
  assert_has_code(&err, "NJS0009");
}

#[test]
fn rejects_nested_function_declaration() {
  let err = validate(
    r#"
      function outer(x: number): number {
        function inner(y: number): number {
          return y + 1;
        }
        return x;
      }

      outer(1);
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
