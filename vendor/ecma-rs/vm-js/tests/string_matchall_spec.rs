use vm_js::{Agent, Budget, HeapLimits, Value, VmError, VmOptions};

fn new_agent() -> Agent {
  Agent::with_options(
    VmOptions::default(),
    // Large enough that these small scripts won't accidentally trip GC paths that could obscure
    // the ordering we want to test.
    HeapLimits::new(1024 * 1024, 1024 * 1024),
  )
  .expect("create agent")
}

#[test]
fn matchall_require_object_coercible_this() -> Result<(), VmError> {
  let mut agent = new_agent();

  let value = agent.run_script(
    "matchall_require_object_coercible.js",
    r#"
      function isTypeError(fn) {
        try { fn(); } catch (e) { return e instanceof TypeError; }
        return false;
      }

      var ok = true;
      ok = ok && isTypeError(function() { String.prototype.matchAll.call(null); });
      ok = ok && isTypeError(function() { String.prototype.matchAll.call(undefined); });
      ok;
    "#,
    Budget::unlimited(1),
    None,
  )?;
  assert_eq!(value, Value::Bool(true));

  Ok(())
}

#[test]
fn matchall_does_not_box_primitive_arguments_for_symbol_dispatch() -> Result<(), VmError> {
  let mut agent = new_agent();

  let value = agent.run_script(
    "matchall_primitive_argument_symbol_dispatch.js",
    r#"
      function thrower() { throw 123; }

      // If the engine boxes primitives for GetMethod, these accessors will run.
      Object.defineProperty(Boolean.prototype, Symbol.matchAll, { configurable: true, get: thrower });
      Object.defineProperty(Number.prototype,  Symbol.matchAll, { configurable: true, get: thrower });
      Object.defineProperty(String.prototype,  Symbol.matchAll, { configurable: true, get: thrower });
      Object.defineProperty(BigInt.prototype,  Symbol.matchAll, { configurable: true, get: thrower });

      "a,b,c".matchAll(",");
      "abc".matchAll(true);
      "abc".matchAll(1);
      "abc".matchAll("a");
      Array.from("a1b1c".matchAll(1n));

      true;
    "#,
    Budget::unlimited(1),
    None,
  )?;
  assert_eq!(value, Value::Bool(true));

  Ok(())
}

#[test]
fn matchall_dispatch_and_flags_checks_happen_before_tostring_receiver() -> Result<(), VmError> {
  let mut agent = new_agent();

  let value = agent.run_script(
    "matchall_ordering.js",
    r#"
      // 1) If `regexp[@@matchAll]` exists, it must be called before ToString(this).
      var receiver = {
        [Symbol.toPrimitive]: function() { throw 999; }
      };
      var called = false;
      var arg0;
      var matcher = {
        [Symbol.matchAll]: function(o) {
          called = true;
          arg0 = o;
          return "ok";
        }
      };
      var res1 = String.prototype.matchAll.call(receiver, matcher);
      var ok1 = (res1 === "ok") && called && (arg0 === receiver);

      // 2) For RegExp arguments, the global-flags check must happen before ToString(this).
      // A non-global RegExp must throw TypeError without invoking receiver.toString/toPrimitive.
      var receiver2 = {
        [Symbol.toPrimitive]: function() { throw 888; }
      };
      var ok2 = false;
      try {
        String.prototype.matchAll.call(receiver2, new RegExp("a"));
      } catch (e) {
        ok2 = (e instanceof TypeError);
      }

      ok1 && ok2;
    "#,
    Budget::unlimited(1),
    None,
  )?;
  assert_eq!(value, Value::Bool(true));

  Ok(())
}
