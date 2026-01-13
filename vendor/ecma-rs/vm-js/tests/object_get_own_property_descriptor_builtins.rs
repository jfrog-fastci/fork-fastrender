use vm_js::{Agent, Budget, HeapLimits, Value, VmError, VmOptions};

fn new_agent() -> Agent {
  Agent::with_options(
    VmOptions::default(),
    HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024),
  )
  .expect("create agent")
}

#[test]
fn object_get_own_property_descriptor_builtins_prototype_attributes() -> Result<(), VmError> {
  let mut agent = new_agent();

  let value = agent.run_script(
    "object_get_own_property_descriptor_builtins.js",
    r#"
      function checkCtorPrototypeAttrs(ctor) {
        const d = Object.getOwnPropertyDescriptor(ctor, "prototype");
        if (d.writable !== false) throw new Error(ctor.name + ".prototype writable");
        if (d.enumerable !== false) throw new Error(ctor.name + ".prototype enumerable");
        if (d.configurable !== false) throw new Error(ctor.name + ".prototype configurable");
      }

      checkCtorPrototypeAttrs(String);
      checkCtorPrototypeAttrs(Error);
      checkCtorPrototypeAttrs(EvalError);
      checkCtorPrototypeAttrs(RangeError);
      checkCtorPrototypeAttrs(ReferenceError);
      checkCtorPrototypeAttrs(SyntaxError);
      checkCtorPrototypeAttrs(TypeError);
      checkCtorPrototypeAttrs(URIError);

      // Date.prototype methods should exist as non-enumerable configurable functions (ES5 / test262
      // `built-ins/Object/getOwnPropertyDescriptor/*`).
      const desc = Object.getOwnPropertyDescriptor(Date.prototype, "getFullYear");
      if (desc.writable !== true) throw new Error("Date.prototype.getFullYear writable");
      if (desc.enumerable !== false) throw new Error("Date.prototype.getFullYear enumerable");
      if (desc.configurable !== true) throw new Error("Date.prototype.getFullYear configurable");
      if (desc.value !== Date.prototype.getFullYear) throw new Error("Date.prototype.getFullYear identity");

      if (typeof Date.prototype.getTimezoneOffset !== "function") throw new Error("missing getTimezoneOffset");
      if (typeof Date.prototype.toJSON !== "function") throw new Error("missing toJSON");

      1;
    "#,
    Budget::unlimited(1),
    None,
  )?;

  assert_eq!(value, Value::Number(1.0));
  Ok(())
}

