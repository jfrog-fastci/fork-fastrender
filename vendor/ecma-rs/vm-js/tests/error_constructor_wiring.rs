use vm_js::{Agent, Budget, HeapLimits, Value, VmError, VmOptions};

fn new_agent() -> Agent {
  Agent::with_options(
    VmOptions::default(),
    HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024),
  )
  .expect("create agent")
}

#[test]
fn restricted_function_property_throws_type_error_with_correct_constructor() -> Result<(), VmError> {
  let mut agent = new_agent();

  // The test262 suite loads a number of harness files before running each test, including
  // `wellKnownIntrinsicObjects.js` which does a large amount of reflective probing via
  // `new Function(...)`.
  //
  // Include that harness here to ensure `%ThrowTypeError%` and Error constructor/prototype wiring
  // remain correct after the VM has executed non-trivial code.
  let sta_harness = include_str!("../../test262-semantic/data/harness/sta.js");
  let assert_harness = include_str!("../../test262-semantic/data/harness/assert.js");
  let property_helper_harness =
    include_str!("../../test262-semantic/data/harness/propertyHelper.js");
  let well_known_intrinsics_harness =
    include_str!("../../test262-semantic/data/harness/wellKnownIntrinsicObjects.js");

  // Encode the key observations into a bitmask so we can debug wiring issues without needing a
  // console.
  //
  // Expected mask:
  // - The thrown value is a TypeError instance with `constructor === TypeError`
  // - Prototypes are wired so `TypeError.prototype.constructor === TypeError`, etc.
  let source = format!(
    "{sta_harness}\n{assert_harness}\n{property_helper_harness}\n{well_known_intrinsics_harness}\n{}",
    r#"
      // Execute the same prelude as test262's `built-ins/Function/prototype/caller/prop-desc.js`
      // before observing the thrown value.
      const callerDesc = Object.getOwnPropertyDescriptor(Function.prototype, 'caller');

      verifyProperty(
        Function.prototype,
        "caller",
        { enumerable: false, configurable: true },
        { restore: true }
      );

      assert.sameValue(typeof callerDesc.get, "function");
      assert.sameValue(typeof callerDesc.set, "function");
      assert.sameValue(callerDesc.get, callerDesc.set);

      // `WellKnownIntrinsicObjects` may fail to obtain `%ThrowTypeError%` on incomplete engines, in
      // which case the test doesn't assert identity. Still run the scan for parity with test262.
      var throwTypeError;
      WellKnownIntrinsicObjects.forEach(function(record) {
        if (record.name === "%ThrowTypeError%") {
          throwTypeError = record.value;
        }
      });
      if (throwTypeError) {
        assert.sameValue(callerDesc.set, throwTypeError);
      }

      let mask = 0;

      function runner(func) {
        func();
      }

      try {
        runner(function() { return Function.prototype.caller; });
      } catch (e) {
        if (e instanceof TypeError) mask = mask + 1;
        if (e instanceof ReferenceError) mask = mask + 2;
        if (e.constructor === TypeError) mask = mask + 4;
        if (e.constructor === ReferenceError) mask = mask + 8;
        if (Object.getPrototypeOf(e) === TypeError.prototype) mask = mask + 16;
        if (Object.getPrototypeOf(e) === ReferenceError.prototype) mask = mask + 32;
      }

      if (TypeError.prototype.constructor === TypeError) mask = mask + 64;
      if (TypeError.prototype.constructor === ReferenceError) mask = mask + 128;
      if (ReferenceError.prototype.constructor === ReferenceError) mask = mask + 256;
      if (ReferenceError.prototype.constructor === TypeError) mask = mask + 512;

      mask;
    "#
  );

  let value = agent.run_script(
    "error_constructor_wiring.js",
    source.as_str(),
    Budget::unlimited(1),
    None,
  )?;

  assert_eq!(value, Value::Number(341.0));
  Ok(())
}

#[test]
fn test262_function_prototype_caller_prop_desc() -> Result<(), VmError> {
  let sta = include_str!("../../test262-semantic/data/harness/sta.js");
  let assert_harness = include_str!("../../test262-semantic/data/harness/assert.js");
  let property_helper = include_str!("../../test262-semantic/data/harness/propertyHelper.js");
  let well_known = include_str!("../../test262-semantic/data/harness/wellKnownIntrinsicObjects.js");
  let test =
    include_str!("../../test262-semantic/data/test/built-ins/Function/prototype/caller/prop-desc.js");

  let combined = format!("{sta}\n{assert_harness}\n{property_helper}\n{well_known}\n{test}\n");

  // Non-strict variant.
  {
    let mut agent = new_agent();
    let value = agent.run_script(
      "test262_function_prototype_caller_prop_desc_non_strict.js",
      combined.as_str(),
      Budget::unlimited(1),
      None,
    )?;
    assert_eq!(value, Value::Undefined);
  }

  // Strict variant.
  {
    let mut agent = new_agent();
    let strict = format!("\"use strict\";\n{combined}");
    let value = agent.run_script(
      "test262_function_prototype_caller_prop_desc_strict.js",
      strict.as_str(),
      Budget::unlimited(1),
      None,
    )?;
    assert_eq!(value, Value::Undefined);
  }

  Ok(())
}

#[test]
fn debug_test262_caller_prop_desc_throws_constructor() -> Result<(), VmError> {
  let sta = include_str!("../../test262-semantic/data/harness/sta.js");
  let assert_harness = include_str!("../../test262-semantic/data/harness/assert.js");
  let property_helper = include_str!("../../test262-semantic/data/harness/propertyHelper.js");
  let well_known = include_str!("../../test262-semantic/data/harness/wellKnownIntrinsicObjects.js");
  let test =
    include_str!("../../test262-semantic/data/test/built-ins/Function/prototype/caller/prop-desc.js");

  // Override `assert.throws` so the script can complete and report the constructor observed for
  // each throw site:
  // - 1 = TypeError
  // - 2 = ReferenceError
  // - 3 = other
  // - 0 = no exception thrown
  let override_throws = r#"
    var __throwResults = [];
    assert.throws = function(expectedErrorConstructor, func, message) {
      try {
        func();
        __throwResults.push(0);
      } catch (thrown) {
        if (thrown && thrown.constructor === TypeError) {
          __throwResults.push(1);
        } else if (thrown && thrown.constructor === ReferenceError) {
          __throwResults.push(2);
        } else {
          __throwResults.push(3);
        }
      }
    };
  "#;

  let combined = format!(
    "{sta}\n{assert_harness}\n{override_throws}\n{property_helper}\n{well_known}\n{test}\n__throwResults[0] * 10 + __throwResults[1];\n"
  );

  let mut agent = new_agent();
  let value = agent.run_script(
    "debug_test262_caller_prop_desc_throws_constructor.js",
    combined.as_str(),
    Budget::unlimited(1),
    None,
  )?;

  // Both throw sites should throw TypeError.
  assert_eq!(value, Value::Number(11.0));
  Ok(())
}
