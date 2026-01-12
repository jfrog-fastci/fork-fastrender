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
fn with_statement_unscopables_blocks_delete_identifier() -> Result<(), VmError> {
  let mut agent = new_gc_stress_agent();
  let value = agent.run_script(
    "with_unscopables_delete.js",
    r#"
      let o = { x: 1 };
      o[Symbol.unscopables] = { x: true };
      let x = 2;
      var r;
      with (o) { r = delete x; }
      String(r) + "|" + o.x + "|" + x
    "#,
    Budget::unlimited(1),
    None,
  )?;
  let out = agent.value_to_error_string(value);
  assert_eq!(out, "false|1|2");
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
fn with_statement_proxy_unscopables_blocks_delete_identifier() -> Result<(), VmError> {
  let mut agent = new_gc_stress_agent();
  let value = agent.run_script(
    "with_proxy_unscopables_delete.js",
    r#"
       var log = [];
       var target = { x: 1 };
       target[Symbol.unscopables] = { x: true };
       var p = new Proxy(target, {
         has(t, k) { log.push("has:" + String(k)); return (k in t); },
         get(t, k, r) { log.push("get:" + String(k) + ":" + (r === p)); return t[k]; },
         deleteProperty(t, k) { log.push("del:" + String(k)); return delete t[k]; },
       });
       let x = 2;
       var r;
       with (p) { r = delete x; }
       String(r) + "|" + log.join(",") + "|" + target.x + "|" + x
     "#,
    Budget::unlimited(1),
    None,
  )?;

  let out = agent.value_to_error_string(value);
  assert!(
    out.starts_with("false|"),
    "expected delete to target the outer binding (unscopables block), got {out}"
  );
  assert!(
    out.ends_with("|1|2"),
    "expected with binding object's property to remain and outer binding to be unchanged, got {out}"
  );
  assert!(
    out.contains("has:x"),
    "expected Proxy has trap to be observed, got {out}"
  );
  assert!(
    out.contains("Symbol.unscopables"),
    "expected @@unscopables Get to be observable, got {out}"
  );
  assert!(
    out.contains("Symbol.unscopables):true"),
    "expected @@unscopables Get receiver to be the binding object, got {out}"
  );
  assert!(
    !out.contains("del:x"),
    "expected Proxy deleteProperty trap to NOT run when unscopables blocks identifier resolution, got {out}"
  );
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

#[test]
fn with_statement_proxy_traps_are_observable() -> Result<(), VmError> {
  let mut agent = new_agent();
  let value = agent.run_script(
    "with_proxy_traps.js",
    r#"
       var log = [];
       var p = new Proxy({ x: 1 }, {
         has(t, k) { log.push("has:" + String(k)); return (k in t); },
         get(t, k, r) { log.push("get:" + String(k) + ":" + (r === p)); return t[k]; },
         // `Reflect.set` is not fully Proxy-receiver-aware in vm-js yet, so avoid forwarding the
         // receiver through it here. This test is specifically asserting that the receiver passed
         // to the Proxy trap is the binding object (`p`) as required by ObjectEnvironmentRecord.
         set(t, k, v, r) { log.push("set:" + String(k) + ":" + (r === p)); t[k] = v; return true; },
         deleteProperty(t, k) { log.push("del:" + String(k)); return delete t[k]; },
       });
       with (p) { x; x = 2; delete x; }
       log.join(",")
     "#,
    Budget::unlimited(1),
    None,
  )?;

  let log = agent.value_to_error_string(value);
  assert!(log.contains("has:x"), "expected Proxy has trap to be observed, got {log}");
  assert!(log.contains("get:x:true"), "expected Proxy get receiver to be the binding object, got {log}");
  assert!(log.contains("set:x:true"), "expected Proxy set receiver to be the binding object, got {log}");
  assert!(log.contains("del:x"), "expected Proxy deleteProperty trap to be observed, got {log}");
  Ok(())
}

#[test]
fn with_statement_proxy_unscopables_get_is_observable() -> Result<(), VmError> {
  let mut agent = new_gc_stress_agent();
  let value = agent.run_script(
    "with_proxy_unscopables_trap.js",
    r#"
       var log = [];
       var target = { x: 1 };
       target[Symbol.unscopables] = { x: true };
       var p = new Proxy(target, {
         get(t, k, r) { log.push("get:" + String(k) + ":" + (r === p)); return t[k]; },
       });
       let x = 2;
       var t;
       with (p) { t = typeof x; }
      t + "|" + log.join(",")
    "#,
    Budget::unlimited(1),
    None,
  )?;

  let out = agent.value_to_error_string(value);
  assert!(
    out.starts_with("number|"),
    "expected unscopables to force identifier resolution to outer binding, got {out}"
  );
  assert!(
    out.contains("Symbol.unscopables"),
    "expected @@unscopables Get to be observable via Proxy get trap, got {out}"
  );
  assert!(
    out.contains("Symbol.unscopables):true"),
    "expected @@unscopables Get receiver to be the binding object, got {out}"
  );
  Ok(())
}

#[test]
fn with_statement_proxy_forwarded_accessor_uses_proxy_receiver() -> Result<(), VmError> {
  let mut agent = new_agent();
  let value = agent.run_script(
    "with_proxy_forwarded_accessor_receiver.js",
    r#"
      var log = [];
      var target = {
        get x() { log.push("getthis:" + (this === p)); return 1; },
        set x(v) { log.push("setthis:" + (this === p)); },
      };
      var p = new Proxy(target, {
        has(t, k) { log.push("has:" + String(k)); return (k in t); },
        // No `get`/`set` trap: engine must forward to target with receiver = proxy.
      });
      with (p) { x; x = 2; }
      log.join(",")
    "#,
    Budget::unlimited(1),
    None,
  )?;
  let log = agent.value_to_error_string(value);
  assert!(log.contains("has:x"), "expected Proxy has trap to be observed, got {log}");
  assert!(
    log.contains("getthis:true"),
    "expected forwarded getter to see receiver === proxy, got {log}"
  );
  assert!(
    log.contains("setthis:true"),
    "expected forwarded setter to see receiver === proxy, got {log}"
  );
  Ok(())
}

#[test]
fn with_statement_proxy_forwarded_unscopables_accessor_uses_proxy_receiver() -> Result<(), VmError> {
  let mut agent = new_gc_stress_agent();
  let value = agent.run_script(
    "with_proxy_forwarded_unscopables_receiver.js",
    r#"
      var log = [];
      var target = { x: 1 };
      Object.defineProperty(target, Symbol.unscopables, {
        get() { log.push("unscopablesThis:" + (this === p)); return { x: true }; },
        configurable: true,
      });
      var p = new Proxy(target, {
        has(t, k) { log.push("has:" + String(k)); return (k in t); },
        // No `get` trap: @@unscopables must still be read with receiver = proxy.
      });
      let x = 2;
      var t;
      with (p) { t = typeof x; }
      t + "|" + log.join(",")
    "#,
    Budget::unlimited(1),
    None,
  )?;
  let out = agent.value_to_error_string(value);
  assert!(
    out.starts_with("number|"),
    "expected unscopables to force identifier resolution to outer binding, got {out}"
  );
  assert!(
    out.contains("has:x"),
    "expected Proxy has trap to be observed, got {out}"
  );
  assert!(
    out.contains("unscopablesThis:true"),
    "expected @@unscopables getter to see receiver === proxy, got {out}"
  );
  Ok(())
}
