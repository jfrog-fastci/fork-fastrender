use std::collections::HashMap;
use std::sync::Arc;

use diagnostics::Diagnostic;
use native_js::builtins::checked_builtins_lib;
use native_js::validate::validate_strict_subset;
use typecheck_ts::lib_support::{CompilerOptions as TsCompilerOptions, FileKind, LibFile, LibName};
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

  fn compiler_options(&self) -> TsCompilerOptions {
    // Mirror the `native-js` checked pipeline defaults: load only the target ES lib, not the DOM
    // lib bundle (which also defines a global `print()` overload).
    TsCompilerOptions {
      libs: vec![LibName::parse("es5").expect("LibName::parse(es5)")],
      ..Default::default()
    }
  }

  fn lib_files(&self) -> Vec<LibFile> {
    // Match the `native-js` checked pipeline: use the canonical intrinsic `.d.ts` lib key and
    // layer any extra test-only ambient declarations on top.
    vec![
      checked_builtins_lib(),
      LibFile {
        key: FileKey::new("native-js:test-builtins.d.ts"),
        name: Arc::from("native-js test builtins"),
        kind: FileKind::Dts,
        // `arguments` is a magic per-function binding in JS; we only declare it so `typecheck-ts`
        // accepts samples that reference it and the strict-subset validator can produce the intended
        // `NJS0009` diagnostic.
        text: Arc::from("declare const arguments: number;\n"),
      },
    ]
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
fn rejects_for_in_loop() {
  let err = validate("const obj: any = 0 as any;\nfor (const k in obj) { k; }\n", FileKind::Ts).unwrap_err();
  assert_has_code(&err, "NJS0009");
}

#[test]
fn accepts_non_i32_numeric_literals() {
  let ok = validate(
    "const a: number = 1.5;\nconst b: number = 1e3;\nconst c: number = 2147483648;\nconst d: number = 0x1_0000_0000;\na;\nb;\nc;\nd;\n",
    FileKind::Ts,
  );
  assert!(ok.is_ok(), "expected strict-subset validation to pass, got: {ok:#?}");
}

#[test]
fn accepts_array_literal() {
  let ok = validate("const xs = [1, 2];\nxs;\n", FileKind::Ts);
  assert!(ok.is_ok(), "expected strict-subset validation to pass, got: {ok:#?}");
}

#[test]
fn rejects_array_literal_holes() {
  let err = validate("const xs = [1, , 2];\nxs;\n", FileKind::Ts).unwrap_err();
  assert_has_code(&err, "NJS0009");
}

#[test]
fn rejects_array_literal_spread() {
  let err = validate(
    "const ys = [1, 2];\nconst xs = [0, ...ys];\nxs;\n",
    FileKind::Ts,
  )
  .unwrap_err();
  assert_has_code(&err, "NJS0009");
}

#[test]
fn accepts_array_indexing_and_length() {
  // Note: member-expression type inference is currently more complete in function bodies than in
  // the root/module body, so exercise array indexing + `.length` inside a function.
  let ok = validate(
    "export function f(): number { const xs = [1, 2]; return xs[0] + xs.length; }\n",
    FileKind::Ts,
  );
  assert!(ok.is_ok(), "expected strict-subset validation to pass, got: {ok:#?}");
}

#[test]
fn rejects_array_non_length_member_access() {
  let err = validate("const xs = [1, 2];\nxs.foo;\n", FileKind::Ts).unwrap_err();
  assert_has_code(&err, "NJS0009");
}

#[test]
fn rejects_conditional_expression() {
  let err = validate("const x = true ? 1 : 2;\nx;\n", FileKind::Ts).unwrap_err();
  assert_has_code(&err, "NJS0009");
}

#[test]
fn accepts_logical_or() {
  let ok = validate(
    r#"
      const a: boolean = true;
      const b: boolean = false;
      const x = a || b;
      x;
    "#,
    FileKind::Ts,
  );
  assert!(ok.is_ok(), "expected strict-subset validation to pass, got: {ok:#?}");
}

#[test]
fn accepts_logical_and() {
  let ok = validate(
    r#"
      const a: boolean = true;
      const b: boolean = false;
      const x = a && b;
      x;
    "#,
    FileKind::Ts,
  );
  assert!(ok.is_ok(), "expected strict-subset validation to pass, got: {ok:#?}");
}

#[test]
fn accepts_comma_operator() {
  let ok = validate(
    r#"
      let a: number = 1;
      let b: number = 2;
      const x = (a, b);
      x;
    "#,
    FileKind::Ts,
  );
  assert!(ok.is_ok(), "expected strict-subset validation to pass, got: {ok:#?}");
}

#[test]
fn accepts_unsigned_shift_right() {
  let ok = validate(
    r#"
      const x = 1 >>> 1;
      x;
    "#,
    FileKind::Ts,
  );
  assert!(ok.is_ok(), "expected strict-subset validation to pass, got: {ok:#?}");
}

#[test]
fn rejects_exponent_operator() {
  let err = validate("const x = 2 ** 3;\nx;\n", FileKind::Ts).unwrap_err();
  assert_has_code(&err, "NJS0009");
}

#[test]
fn rejects_member_access() {
  let err = validate("const x = Math.PI;\nx;\n", FileKind::Ts).unwrap_err();
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
        arguments;
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
fn accepts_user_defined_print_in_expression_position() {
  let ok = validate(
    r#"
      function print(x: number): number {
        return x + 1;
      }

      function run(): number {
        return print(41);
      }

      export {};
    "#,
    FileKind::Ts,
  );
  assert!(ok.is_ok(), "expected strict-subset validation to pass, got: {ok:#?}");
}

#[test]
fn rejects_optional_parameter() {
  let err = validate(
    r#"
      function f(x?: number): number {
        return 0;
      }
      f(1);
    "#,
    FileKind::Ts,
  )
  .unwrap_err();
  assert_has_code(&err, "NJS0009");
}

#[test]
fn rejects_default_parameter() {
  let err = validate(
    r#"
      function f(x: number = 1): number {
        return x;
      }
      f(1);
    "#,
    FileKind::Ts,
  )
  .unwrap_err();
  assert_has_code(&err, "NJS0009");
}

#[test]
fn rejects_rest_parameter() {
  let err = validate(
    r#"
      function f(...xs: number[]): number {
        return 0;
      }
      f(1);
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
fn rejects_void_module_global() {
  let err = validate(
    r#"
      function f(): void {
      }
      const x = f();
      x;
    "#,
    FileKind::Ts,
  )
  .unwrap_err();
  assert_has_code(&err, "NJS0010");
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

#[test]
fn rejects_lying_type_assertion() {
  let err = validate("const x = (1 as unknown as boolean);\nx;\n", FileKind::Ts).unwrap_err();
  assert_has_code(&err, "NJS0013");
}

#[test]
fn rejects_unsafe_non_null() {
  let err = validate(
    r#"
      function f(): void {}
      const x = f();
      x!;
    "#,
    FileKind::Ts,
  )
  .unwrap_err();
  assert_has_code(&err, "NJS0014");
}

#[test]
fn accepts_non_null_after_check() {
  let ok = validate(
    r#"
      const x: number = 1;
      if (x !== 0) {
        x!;
      }
      x;
    "#,
    FileKind::Ts,
  );
  assert!(ok.is_ok(), "expected strict-subset validation to pass, got: {ok:#?}");
}
