use vm_js::{Agent, Budget, HeapLimits, Value, VmError, VmOptions};

fn new_agent() -> Agent {
  Agent::with_options(
    VmOptions::default(),
    HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024),
  )
  .expect("create agent")
}

fn new_gc_stress_agent() -> Agent {
  // Force a GC before each allocation to stress rooting in `with` binding resolution (notably
  // `@@unscopables` handling).
  Agent::with_options(
    VmOptions::default(),
    HeapLimits::new(16 * 1024 * 1024, 0),
  )
  .expect("create agent")
}

#[test]
fn with_statement_reads_properties() -> Result<(), VmError> {
  let mut agent = new_agent();
  let value = agent.run_script(
    "with_read.js",
    "with ({ a: 1 }) { a; }",
    Budget::unlimited(1),
    None,
  )?;
  assert_eq!(value, Value::Number(1.0));
  Ok(())
}

#[test]
fn with_statement_writes_properties() -> Result<(), VmError> {
  let mut agent = new_agent();
  let value = agent.run_script(
    "with_write.js",
    "let o = { a: 0 }; with (o) { a = 3; } o.a;",
    Budget::unlimited(1),
    None,
  )?;
  assert_eq!(value, Value::Number(3.0));
  Ok(())
}

#[test]
fn with_statement_respects_unscopables() -> Result<(), VmError> {
  let mut agent = new_gc_stress_agent();
  let value = agent.run_script(
    "with_unscopables.js",
    r#"
      let o = { x: 1 };
      o[Symbol.unscopables] = { x: true };
      let x = 2;
      with (o) { x; }
    "#,
    Budget::unlimited(1),
    None,
  )?;
  assert_eq!(value, Value::Number(2.0));
  Ok(())
}

#[test]
fn with_statement_delete_identifier_deletes_property() -> Result<(), VmError> {
  let mut agent = new_agent();
  let value = agent.run_script(
    "with_delete.js",
    "with ({ x: 1 }) { delete x; }",
    Budget::unlimited(1),
    None,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn with_statement_var_declared_names_are_hoisted() -> Result<(), VmError> {
  let mut agent = new_agent();
  let value = agent.run_script(
    "with_var_hoist.js",
    "var ok = (x === void 0); with ({}) { var x = 1; } ok;",
    Budget::unlimited(1),
    None,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn with_statement_sloppy_block_function_decls_are_var_scoped() -> Result<(), VmError> {
  let mut agent = new_agent();
  let value = agent.run_script(
    "with_block_fn.js",
    "with ({}) { function f() { return 7; } } f();",
    Budget::unlimited(1),
    None,
  )?;
  assert_eq!(value, Value::Number(7.0));
  Ok(())
}

#[test]
fn strict_mode_with_is_syntax_error_and_does_not_pollute_var_env() {
  let mut agent = new_agent();

  let err = agent
    .run_script(
      "strict_with.js",
      r#""use strict"; var y = 1; with ({}) {}"#,
      Budget::unlimited(1),
      None,
    )
    .expect_err("strict-mode with should be a syntax error");
  assert!(matches!(err, VmError::Syntax(_)));

  // The failed script must not have hoisted `y` into the global var environment.
  let err = agent
    .run_script("after_strict_with.js", "y", Budget::unlimited(1), None)
    .expect_err("y should remain unbound after failed instantiation");
  assert!(matches!(err, VmError::ThrowWithStack { .. }));
}

