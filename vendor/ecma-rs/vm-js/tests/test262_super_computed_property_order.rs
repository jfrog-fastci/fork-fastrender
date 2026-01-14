use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // test262 harness + these scripts are small, but give them a little extra heap to avoid spurious
  // OOMs when running both interpreter + compiled paths.
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

const HARNESS: &str = concat!(
  include_str!("../../test262-semantic/data/harness/assert.js"),
  "\n",
  include_str!("../../test262-semantic/data/harness/sta.js"),
  "\n",
);

fn run_test_in_interpreter(name: &str, body: &str) -> Result<Value, VmError> {
  let mut rt = new_runtime();
  let source = format!("{HARNESS}\n{body}\n");
  rt.exec_script(&source).map_err(|err| {
    panic!("interpreter failed for {name} with error: {err:?}\n\nsource:\n{source}");
  })
}

fn run_test_in_compiled(name: &str, body: &str) -> Result<Value, VmError> {
  let mut rt = new_runtime();
  let source = format!("{HARNESS}\n{body}\n");
  let script = CompiledScript::compile_script(rt.heap_mut(), name, &source).map_err(|err| {
    panic!("compile failed for {name} with error: {err:?}\n\nsource:\n{source}");
  })?;
  assert!(
    !script.requires_ast_fallback,
    "test script should execute via compiled (HIR) executor: {name}"
  );
  rt.exec_compiled_script(script).map_err(|err| {
    panic!("compiled execution failed for {name} with error: {err:?}\n\nsource:\n{source}");
  })
}

fn assert_undefined(value: Value, name: &str, mode: &str) {
  assert!(
    matches!(value, Value::Undefined),
    "{mode} returned unexpected value for {name}: {value:?}"
  );
}

#[test]
fn test262_super_computed_property_ordering() -> Result<(), VmError> {
  // GetSuperBase must be observed before ToPropertyKey (key coercion may mutate prototype) and
  // GetThisBinding must run before key expression evaluation (derived constructors before
  // `super()` throw early without running the key expression).
  const TESTS: &[(&str, &str)] = &[
    (
      "prop-expr-getsuperbase-before-topropertykey-getvalue.js",
      include_str!(
        "../../test262-semantic/data/test/language/expressions/super/prop-expr-getsuperbase-before-topropertykey-getvalue.js"
      ),
    ),
    (
      "prop-expr-getsuperbase-before-topropertykey-putvalue.js",
      include_str!(
        "../../test262-semantic/data/test/language/expressions/super/prop-expr-getsuperbase-before-topropertykey-putvalue.js"
      ),
    ),
    (
      "prop-expr-getsuperbase-before-topropertykey-putvalue-increment.js",
      include_str!(
        "../../test262-semantic/data/test/language/expressions/super/prop-expr-getsuperbase-before-topropertykey-putvalue-increment.js"
      ),
    ),
    (
      "prop-expr-getsuperbase-before-topropertykey-putvalue-compound-assign.js",
      include_str!(
        "../../test262-semantic/data/test/language/expressions/super/prop-expr-getsuperbase-before-topropertykey-putvalue-compound-assign.js"
      ),
    ),
    (
      "prop-expr-uninitialized-this-getvalue.js",
      include_str!(
        "../../test262-semantic/data/test/language/expressions/super/prop-expr-uninitialized-this-getvalue.js"
      ),
    ),
    (
      "prop-expr-uninitialized-this-putvalue.js",
      include_str!(
        "../../test262-semantic/data/test/language/expressions/super/prop-expr-uninitialized-this-putvalue.js"
      ),
    ),
    (
      "prop-expr-uninitialized-this-putvalue-increment.js",
      include_str!(
        "../../test262-semantic/data/test/language/expressions/super/prop-expr-uninitialized-this-putvalue-increment.js"
      ),
    ),
    (
      "prop-expr-uninitialized-this-putvalue-compound-assign.js",
      include_str!(
        "../../test262-semantic/data/test/language/expressions/super/prop-expr-uninitialized-this-putvalue-compound-assign.js"
      ),
    ),
  ];

  for (name, body) in TESTS {
    let value = run_test_in_interpreter(name, body)?;
    assert_undefined(value, name, "interpreter");

    let value = run_test_in_compiled(name, body)?;
    assert_undefined(value, name, "compiled");
  }

  Ok(())
}

