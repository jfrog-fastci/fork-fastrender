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
fn rejects_eval_call_via_function_call_property() {
  let src = r#"
    eval.call(null, "1 + 1");
  "#;

  let err = optimize_js::compile_source_typed_strict_native(
    src,
    TopLevelMode::Module,
    false,
    StrictNativeOpts::default(),
  )
  .expect_err("eval.call should be rejected");

  assert!(
    err.iter().any(|d| d.code == "OPTN0005" && d.message.contains("eval.call")),
    "expected OPTN0005 diagnostic mentioning eval.call, got {err:?}"
  );
}

#[test]
fn rejects_eval_via_function_prototype_call_call() {
  let src = r#"
    Function.prototype.call.call(eval, null, "1 + 1");
  "#;

  let err = optimize_js::compile_source_typed_strict_native(
    src,
    TopLevelMode::Module,
    false,
    StrictNativeOpts::default(),
  )
  .expect_err("Function.prototype.call.call(eval, ...) should be rejected");

  assert!(
    err.iter().any(|d| d.code == "OPTN0005" && d.message.contains("eval")),
    "expected OPTN0005 diagnostic mentioning eval, got {err:?}"
  );
}

#[test]
fn rejects_eval_via_reflect_apply() {
  let src = r#"
    Reflect.apply(eval, null, ["1 + 1"]);
  "#;

  let err = optimize_js::compile_source_typed_strict_native(
    src,
    TopLevelMode::Module,
    false,
    StrictNativeOpts::default(),
  )
  .expect_err("Reflect.apply(eval, ...) should be rejected");

  assert!(
    err.iter().any(|d| d.code == "OPTN0005" && d.message.contains("eval")),
    "expected OPTN0005 diagnostic mentioning eval, got {err:?}"
  );
}

#[test]
fn rejects_eval_via_reflect_apply_function_prototype_call() {
  let src = r#"
    Reflect.apply(Function.prototype.call, eval, [null, "1 + 1"]);
  "#;

  let err = optimize_js::compile_source_typed_strict_native(
    src,
    TopLevelMode::Module,
    false,
    StrictNativeOpts::default(),
  )
  .expect_err("Reflect.apply(Function.prototype.call, eval, ...) should be rejected");

  assert!(
    err.iter().any(|d| d.code == "OPTN0005" && d.message.contains("eval")),
    "expected OPTN0005 diagnostic mentioning eval, got {err:?}"
  );
}

#[test]
fn rejects_eval_via_reflect_apply_call() {
  let src = r#"
    Reflect.apply.call(Reflect, eval, null, ["1 + 1"]);
  "#;

  let err = optimize_js::compile_source_typed_strict_native(
    src,
    TopLevelMode::Module,
    false,
    StrictNativeOpts::default(),
  )
  .expect_err("Reflect.apply.call(Reflect, eval, ...) should be rejected");

  assert!(
    err.iter().any(|d| d.code == "OPTN0005" && d.message.contains("eval")),
    "expected OPTN0005 diagnostic mentioning eval, got {err:?}"
  );
}

#[test]
fn rejects_eval_via_function_prototype_call_bind() {
  let src = r#"
    Function.prototype.call.bind(eval)(null, "1 + 1");
  "#;

  let err = optimize_js::compile_source_typed_strict_native(
    src,
    TopLevelMode::Module,
    false,
    StrictNativeOpts::default(),
  )
  .expect_err("Function.prototype.call.bind(eval)(...) should be rejected");

  assert!(
    err
      .iter()
      .any(|d| d.code == "OPTN0005" && d.message.contains("binding `eval`")),
    "expected OPTN0005 diagnostic mentioning binding eval, got {err:?}"
  );
}

#[test]
fn rejects_eval_via_reflect_apply_bind() {
  let src = r#"
    Reflect.apply.bind(Reflect, eval, null, ["1 + 1"])();
  "#;

  let err = optimize_js::compile_source_typed_strict_native(
    src,
    TopLevelMode::Module,
    false,
    StrictNativeOpts::default(),
  )
  .expect_err("Reflect.apply.bind(Reflect, eval, ...)(...) should be rejected");

  assert!(
    err
      .iter()
      .any(|d| d.code == "OPTN0005" && d.message.contains("binding `eval`")),
    "expected OPTN0005 diagnostic mentioning binding eval, got {err:?}"
  );
}

#[test]
fn rejects_eval_via_reflect_apply_function_prototype_bind() {
  let src = r#"
    Reflect.apply(Function.prototype.bind, eval, [null, "1 + 1"]);
  "#;

  let err = optimize_js::compile_source_typed_strict_native(
    src,
    TopLevelMode::Module,
    false,
    StrictNativeOpts::default(),
  )
  .expect_err("Reflect.apply(Function.prototype.bind, eval, ...) should be rejected");

  assert!(
    err
      .iter()
      .any(|d| d.code == "OPTN0005" && d.message.contains("binding `eval`")),
    "expected OPTN0005 diagnostic mentioning binding eval, got {err:?}"
  );
}

#[test]
fn rejects_function_call_indirection() {
  let src = r#"
    Function.call(null, "return 1");
  "#;

  let err = optimize_js::compile_source_typed_strict_native(
    src,
    TopLevelMode::Module,
    false,
    StrictNativeOpts::default(),
  )
  .expect_err("Function.call should be rejected");

  assert!(
    err.iter().any(|d| d.code == "OPTN0005" && d.message.contains("Function.call")),
    "expected OPTN0005 diagnostic mentioning Function.call, got {err:?}"
  );
}

#[test]
fn rejects_function_via_reflect_construct_bind() {
  let src = r#"
    Reflect.construct.bind(Reflect, Function, ["return 1"])();
  "#;

  let err = optimize_js::compile_source_typed_strict_native(
    src,
    TopLevelMode::Module,
    false,
    StrictNativeOpts::default(),
  )
  .expect_err("Reflect.construct.bind(Reflect, Function, ...)(...) should be rejected");

  assert!(
    err
      .iter()
      .any(|d| d.code == "OPTN0005" && d.message.contains("constructing `Function`")),
    "expected OPTN0005 diagnostic mentioning constructing Function, got {err:?}"
  );
}

#[test]
fn rejects_function_via_reflect_construct() {
  let src = r#"
    Reflect.construct(Function, ["return 1"]);
  "#;

  let err = optimize_js::compile_source_typed_strict_native(
    src,
    TopLevelMode::Module,
    false,
    StrictNativeOpts::default(),
  )
  .expect_err("Reflect.construct(Function, ...) should be rejected");

  assert!(
    err
      .iter()
      .any(|d| d.code == "OPTN0005" && d.message.contains("constructing `Function`")),
    "expected OPTN0005 diagnostic mentioning constructing Function, got {err:?}"
  );
}

#[test]
fn rejects_proxy_via_reflect_construct() {
  let src = r#"
    Reflect.construct(Proxy, [{}, {}]);
  "#;

  let err = optimize_js::compile_source_typed_strict_native(
    src,
    TopLevelMode::Module,
    false,
    StrictNativeOpts::default(),
  )
  .expect_err("Reflect.construct(Proxy, ...) should be rejected");

  assert!(
    err
      .iter()
      .any(|d| d.code == "OPTN0005" && d.message.contains("constructing `Proxy`")),
    "expected OPTN0005 diagnostic mentioning constructing Proxy, got {err:?}"
  );
}

#[test]
fn rejects_proxy_revocable_calls() {
  let src = r#"
    Proxy.revocable({}, {});
  "#;

  let err = optimize_js::compile_source_typed_strict_native(
    src,
    TopLevelMode::Module,
    false,
    StrictNativeOpts::default(),
  )
  .expect_err("Proxy.revocable should be rejected");

  assert!(
    err
      .iter()
      .any(|d| d.code == "OPTN0005" && d.message.contains("Proxy.revocable")),
    "expected OPTN0005 diagnostic mentioning Proxy.revocable, got {err:?}"
  );
}

#[test]
fn rejects_set_prototype_of_call_indirection() {
  let src = r#"
    const value: object = {};
    Object.setPrototypeOf.call(Object, value, {});
  "#;

  let err = optimize_js::compile_source_typed_strict_native(
    src,
    TopLevelMode::Module,
    false,
    StrictNativeOpts::default(),
  )
  .expect_err("Object.setPrototypeOf.call should be rejected");

  assert!(
    err.iter().any(|d| d.code == "OPTN0005" && d.message.contains("Object.setPrototypeOf.call")),
    "expected OPTN0005 diagnostic mentioning Object.setPrototypeOf.call, got {err:?}"
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
