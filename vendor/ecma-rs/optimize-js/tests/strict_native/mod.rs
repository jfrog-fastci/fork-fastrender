use optimize_js::strict_native::{validate_program, StrictNativeOpts};
use optimize_js::TopLevelMode;

#[test]
fn rejects_computed_property_get() {
  let src = r#"
    function get(obj, key) {
      return obj[key];
    }
    get({ x: 1 }, "x");
  "#;

  let err = optimize_js::compile_source_typed_strict_native(
    src,
    TopLevelMode::Module,
    false,
    StrictNativeOpts::default(),
  )
  .expect_err("computed property access should be rejected");

  assert!(
    err.iter().any(|d| d.code == "OPTN0004"),
    "expected OPTN0004 diagnostic, got {err:?}"
  );
}

#[test]
fn rejects_spread_calls() {
  let src = r#"
    let sink = 0;
    function f(a, b, c) {
      sink = a + b + c;
    }
    const xs = [1, 2, 3];
    f(...xs);
    sink;
  "#;

  let err = optimize_js::compile_source_typed_strict_native(
    src,
    TopLevelMode::Module,
    false,
    StrictNativeOpts::default(),
  )
  .expect_err("spread calls should be rejected");

  assert!(
    err.iter().any(|d| d.code == "OPTN0003"),
    "expected OPTN0003 diagnostic, got {err:?}"
  );
}

#[test]
fn rejects_eval_calls_even_when_indirect() {
  let src = r#"
    const e = eval;
    e("1 + 1");
  "#;

  let err = optimize_js::compile_source_typed_strict_native(
    src,
    TopLevelMode::Module,
    false,
    StrictNativeOpts::default(),
  )
  .expect_err("eval calls should be rejected");

  assert!(
    err.iter().any(|d| d.code == "OPTN0005" && d.message.contains("eval")),
    "expected OPTN0005 diagnostic mentioning eval, got {err:?}"
  );
}

#[test]
fn accepts_constant_property_access() {
  let src = r#"
    let out = 0;
    function sink(v) { out = v; }
    const obj = { x: Math.random() };
    sink(obj["x"]);
    sink(obj.x);
    out;
  "#;

  optimize_js::compile_source_typed_strict_native(
    src,
    TopLevelMode::Module,
    false,
    StrictNativeOpts::default(),
  )
  .expect("constant property access should be allowed");
}

#[test]
fn typed_mode_requires_type_metadata_when_enabled() {
  let src = r#"
    throw {};
  "#;

  let program = optimize_js::compile_source(src, TopLevelMode::Module, false).expect("compile");
  let err = validate_program(
    &program,
    StrictNativeOpts {
      require_type_ids: true,
      ..StrictNativeOpts::default()
    },
  )
  .expect_err("expected missing type ids");

  assert!(
    err.iter().any(|d| d.code == "OPTN0006"),
    "expected OPTN0006 diagnostic, got {err:?}"
  );
  assert!(
    err.iter().all(|d| d.code == "OPTN0006"),
    "expected only OPTN0006 diagnostics, got {err:?}"
  );
}
