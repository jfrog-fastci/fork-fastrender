use optimize_js::compile_source;
use optimize_js::TopLevelMode;

#[test]
fn assert_proven_false_produces_diagnostic() {
  let err = compile_source(
    r#"
      assert(false);
    "#,
    TopLevelMode::Module,
    false,
  )
  .expect_err("expected assert(false) to fail compilation");

  assert!(
    err.iter()
      .any(|diag| diag.code == "OPT0010" && diag.message.contains("assertion")),
    "expected OPT0010 assertion diagnostic, got {err:?}"
  );
}

