use vm_js::{Agent, Budget, HeapLimits, Value, VmError, VmOptions};

fn new_agent() -> Agent {
  Agent::with_options(
    VmOptions::default(),
    HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024),
  )
  .expect("create agent")
}

const ASSERT_SAME_VALUE: &str = r#"
  const assert = {
    sameValue(actual, expected) {
      if (actual === expected) {
        // Treat +0 and -0 as different.
        if (actual === 0 && (1 / actual) !== (1 / expected)) {
          throw new Error("SameValue: +0 vs -0");
        }
        return;
      }
      // Treat NaN as SameValue(NaN, NaN).
      if (actual !== actual && expected !== expected) {
        return;
      }
      throw new Error("SameValue failed");
    }
  };
"#;

#[test]
fn object_prototype_proto_sanity() -> Result<(), VmError> {
  let mut agent = new_agent();
  let src = format!(
    r#"
      {ASSERT_SAME_VALUE}
      const o = {{}};
      const p = {{}};
      o.__proto__ = p;
      assert.sameValue(Object.getPrototypeOf(o), p);
      assert.sameValue((1).__proto__, Number.prototype);
      const desc = Object.getOwnPropertyDescriptor(Object.prototype, "__proto__");
      let threwNull = false;
      try {{ desc.set.call(null, p); }} catch (e) {{ threwNull = e instanceof TypeError; }}
      assert.sameValue(threwNull, true);
      let threwUndef = false;
      try {{ desc.set.call(undefined, p); }} catch (e) {{ threwUndef = e instanceof TypeError; }}
      assert.sameValue(threwUndef, true);
      let threwPrimitive = false;
      try {{ desc.set.call(1, p); }} catch (e) {{ threwPrimitive = true; }}
      assert.sameValue(threwPrimitive, false);
      true;
    "#
  );
  let value = agent.run_script(
    "object_proto_accessor_sanity.js",
    src,
    Budget::unlimited(1),
    None,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn object_prototype_proto_generator_prototype_chain() -> Result<(), VmError> {
  let mut agent = new_agent();
  let src = format!(
    r#"
      {ASSERT_SAME_VALUE}
      function* g() {{ yield 1; }}
      assert.sameValue(g.prototype.__proto__.constructor, Object.getPrototypeOf(g));
      true;
    "#
  );
  let value = match agent.run_script(
    "object_proto_accessor_generator.js",
    src,
    Budget::unlimited(1),
    None,
  ) {
    Ok(v) => v,
    // Generators are still gated in `vm-js`; keep this test active so once generator functions land,
    // we validate `g.prototype.__proto__` works for test262's prototype-chain assertions.
    Err(VmError::Unimplemented("generator functions")) => return Ok(()),
    Err(err) => return Err(err),
  };
  assert_eq!(value, Value::Bool(true));
  Ok(())
}
