use vm_js::{Agent, Budget, HeapLimits, Value, VmError, VmOptions};

fn new_agent() -> Agent {
  Agent::with_options(
    VmOptions::default(),
    HeapLimits::new(1024 * 1024, 1024 * 1024),
  )
  .expect("create agent")
}

fn eval_to_string(agent: &mut Agent, source_name: &str, source_text: &str) -> Result<String, VmError> {
  let v = agent.run_script(source_name, source_text, Budget::unlimited(1), None)?;
  let Value::String(s) = v else {
    return Err(VmError::Unimplemented("expected script to return a string"));
  };
  Ok(agent.heap().get_string(s)?.to_utf8_lossy())
}

#[test]
fn object_prototype_to_string_tags_and_proto_accessors() -> Result<(), VmError> {
  let mut agent = new_agent();

  assert_eq!(
    eval_to_string(
      &mut agent,
      "tostring_array.js",
      "Object.prototype.toString.call([])",
    )?,
    "[object Array]"
  );
  assert_eq!(
    eval_to_string(
      &mut agent,
      "tostring_function.js",
      "Object.prototype.toString.call(function(){})",
    )?,
    "[object Function]"
  );
  assert_eq!(
    eval_to_string(
      &mut agent,
      "tostring_date.js",
      "Object.prototype.toString.call(new Date(0))",
    )?,
    "[object Date]"
  );
  assert_eq!(
    eval_to_string(
      &mut agent,
      "tostring_date_fallback.js",
      r#"(() => {
        const desc = Object.getOwnPropertyDescriptor(Date.prototype, Symbol.toStringTag);
        delete Date.prototype[Symbol.toStringTag];
        const real = Object.prototype.toString.call(new Date(0));
        const fake = Object.prototype.toString.call(Object.create(Date.prototype));
        if (desc) Object.defineProperty(Date.prototype, Symbol.toStringTag, desc);
        return real + "|" + fake;
      })()"#,
    )?,
    "[object Date]|[object Object]"
  );

  assert_eq!(
    eval_to_string(
      &mut agent,
      "tostring_weakmap.js",
      "Object.prototype.toString.call(new WeakMap())",
    )?,
    "[object WeakMap]"
  );
  assert_eq!(
    eval_to_string(
      &mut agent,
      "tostring_weakmap_fallback.js",
      r#"(() => {
        const desc = Object.getOwnPropertyDescriptor(WeakMap.prototype, Symbol.toStringTag);
        delete WeakMap.prototype[Symbol.toStringTag];
        const real = Object.prototype.toString.call(new WeakMap());
        const fake = Object.prototype.toString.call(Object.create(WeakMap.prototype));
        if (desc) Object.defineProperty(WeakMap.prototype, Symbol.toStringTag, desc);
        return real + "|" + fake;
      })()"#,
    )?,
    "[object WeakMap]|[object Object]"
  );

  assert_eq!(
    eval_to_string(
      &mut agent,
      "tostring_weakset.js",
      "Object.prototype.toString.call(new WeakSet())",
    )?,
    "[object WeakSet]"
  );
  assert_eq!(
    eval_to_string(
      &mut agent,
      "tostring_weakset_fallback.js",
      r#"(() => {
        const desc = Object.getOwnPropertyDescriptor(WeakSet.prototype, Symbol.toStringTag);
        delete WeakSet.prototype[Symbol.toStringTag];
        const real = Object.prototype.toString.call(new WeakSet());
        const fake = Object.prototype.toString.call(Object.create(WeakSet.prototype));
        if (desc) Object.defineProperty(WeakSet.prototype, Symbol.toStringTag, desc);
        return real + "|" + fake;
      })()"#,
    )?,
    "[object WeakSet]|[object Object]"
  );

  assert_eq!(
    agent.run_script(
      "proto_get.js",
      "({}).__proto__ === Object.prototype",
      Budget::unlimited(1),
      None,
    )?,
    Value::Bool(true)
  );

  assert_eq!(
    agent.run_script(
      "proto_set_null.js",
      "(() => { const o = {}; o.__proto__ = null; return Object.getPrototypeOf(o) === null; })()",
      Budget::unlimited(1),
      None,
    )?,
    Value::Bool(true)
  );

  assert_eq!(
    agent.run_script(
      "proto_set_invalid.js",
      "(() => { const o = {}; o.__proto__ = 123; return Object.getPrototypeOf(o) === Object.prototype; })()",
      Budget::unlimited(1),
      None,
    )?,
    Value::Bool(true)
  );

  Ok(())
}
