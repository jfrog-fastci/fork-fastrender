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
fn match_search_split_require_object_coercible_this() -> Result<(), VmError> {
  let mut agent = new_agent();

  let value = agent.run_script(
    "require_object_coercible.js",
    r#"
      function isTypeError(fn) {
        try { fn(); } catch (e) { return e instanceof TypeError; }
        return false;
      }

      var ok = true;
      ok = ok && isTypeError(function() { String.prototype.match.call(null); });
      ok = ok && isTypeError(function() { String.prototype.match.call(undefined); });
      ok = ok && isTypeError(function() { String.prototype.search.call(null); });
      ok = ok && isTypeError(function() { String.prototype.search.call(undefined); });
      ok = ok && isTypeError(function() { String.prototype.split.call(null); });
      ok = ok && isTypeError(function() { String.prototype.split.call(undefined); });
      ok;
    "#,
    Budget::unlimited(1),
    None,
  )?;
  assert_eq!(value, Value::Bool(true));

  Ok(())
}

#[test]
fn match_search_split_do_not_box_primitive_arguments_for_symbol_dispatch() -> Result<(), VmError> {
  let mut agent = new_agent();

  let value = agent.run_script(
    "primitive_argument_symbol_dispatch.js",
    r#"
      function thrower() { throw 123; }

      // If the engine boxes primitives for GetMethod, these accessors will run.
      Object.defineProperty(Boolean.prototype, Symbol.match, { configurable: true, get: thrower });
      Object.defineProperty(Number.prototype,  Symbol.match, { configurable: true, get: thrower });
      Object.defineProperty(String.prototype,  Symbol.match, { configurable: true, get: thrower });

      Object.defineProperty(Boolean.prototype, Symbol.search, { configurable: true, get: thrower });
      Object.defineProperty(Number.prototype,  Symbol.search, { configurable: true, get: thrower });
      Object.defineProperty(String.prototype,  Symbol.search, { configurable: true, get: thrower });

      Object.defineProperty(Boolean.prototype, Symbol.split, { configurable: true, get: thrower });
      Object.defineProperty(Number.prototype,  Symbol.split, { configurable: true, get: thrower });
      Object.defineProperty(String.prototype,  Symbol.split, { configurable: true, get: thrower });

      "abc".match(true);
      "abc".match(1);
      "abc".match("a");

      "abc".search(true);
      "abc".search(1);
      "abc".search("a");

      "abc".split(true);
      "abc".split(1);
      "abc".split("b");

      true;
    "#,
    Budget::unlimited(1),
    None,
  )?;
  assert_eq!(value, Value::Bool(true));

  Ok(())
}

#[test]
fn split_tostring_separator_occurs_before_limit_zero_early_return() -> Result<(), VmError> {
  let mut agent = new_agent();

  let value = agent.run_script(
    "split_separator_tostring_ordering.js",
    r#"
      var sep = { toString: function() { throw 123; } };
      var threw = false;
      try {
        "abc".split(sep, 0);
      } catch (e) {
        threw = (e === 123);
      }
      threw;
    "#,
    Budget::unlimited(1),
    None,
  )?;
  assert_eq!(value, Value::Bool(true));

  Ok(())
}

