#![cfg(feature = "typecheck-strict-native")]

use native_oracle_harness::typecheck_strict_native;

fn has_code(diags: &[diagnostics::Diagnostic], code: &str) -> bool {
  diags.iter().any(|diag| diag.code.as_str() == code)
}

#[test]
fn rejects_any() {
  let err = typecheck_strict_native("rejects_any.ts", "const x: any = 1;")
    .expect_err("expected strict-native typecheck to reject `any`");
  assert!(
    has_code(&err, "TC4000"),
    "expected TC4000 diagnostic, got: {err:?}"
  );
}

#[test]
fn rejects_eval() {
  let err = typecheck_strict_native("rejects_eval.ts", "eval(\"1\")")
    .expect_err("expected strict-native typecheck to reject `eval`");
  assert!(
    has_code(&err, "TC4001"),
    "expected TC4001 diagnostic, got: {err:?}"
  );
}

#[test]
fn accepts_simple_program() {
  typecheck_strict_native(
    "accepts_simple_program.ts",
    "const x = 1;\nconst y = x + 1;\nvoid y;",
  )
  .expect("expected strict-native typecheck to accept a simple program");
}

