use vm_js::{Agent, Budget, HeapLimits, Value, VmError, VmOptions};

fn new_agent() -> Agent {
  Agent::with_options(
    VmOptions::default(),
    HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024),
  )
  .expect("create agent")
}

#[test]
fn function_prototype_has_restricted_caller_and_arguments_accessors() -> Result<(), VmError> {
  let mut agent = new_agent();
  let value = agent.run_script(
    "function_prototype_restricted_properties.js",
    r#"
      const descCaller = Object.getOwnPropertyDescriptor(Function.prototype, 'caller');
      const descArgs = Object.getOwnPropertyDescriptor(Function.prototype, 'arguments');

      const okDesc =
        Object.getPrototypeOf(descCaller) === Object.prototype &&
        Object.getPrototypeOf(descArgs) === Object.prototype &&
        typeof descCaller.get === 'function' &&
        descCaller.get === descCaller.set &&
        descCaller.get === descArgs.get &&
        descCaller.get === descArgs.set &&
        descCaller.enumerable === false &&
        descCaller.configurable === true &&
        descArgs.enumerable === false &&
        descArgs.configurable === true;

      let okCallerGet = false;
      try { Function.prototype.caller; } catch (e) { okCallerGet = e instanceof TypeError; }

      let okCallerSet = false;
      try { Function.prototype.caller = 1; } catch (e) { okCallerSet = e instanceof TypeError; }

      let okArgsGet = false;
      try { Function.prototype.arguments; } catch (e) { okArgsGet = e instanceof TypeError; }

      let okArgsSet = false;
      try { Function.prototype.arguments = 1; } catch (e) { okArgsSet = e instanceof TypeError; }

      okDesc && okCallerGet && okCallerSet && okArgsGet && okArgsSet;
    "#,
    Budget::unlimited(1),
    None,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn generator_functions_inherit_function_prototype_restricted_properties() -> Result<(), VmError> {
  let mut agent = new_agent();

  let result = agent.run_script(
    "generator_function_restricted_properties.js",
    r#"
      const GF = Object.getPrototypeOf(function*(){}).constructor;
      const g = new GF();

      const okNoOwn =
        g.hasOwnProperty('caller') === false &&
        g.hasOwnProperty('arguments') === false;

      let okCallerGet = false;
      try { g.caller; } catch (e) { okCallerGet = e instanceof TypeError; }

      let okCallerSet = false;
      try { g.caller = 1; } catch (e) { okCallerSet = e instanceof TypeError; }

      okNoOwn && okCallerGet && okCallerSet;
    "#,
    Budget::unlimited(1),
    None,
  );

  match result {
    Ok(value) => {
      assert_eq!(value, Value::Bool(true));
      Ok(())
    }
    // Generator functions are still unimplemented in vm-js. When implemented, the assertions above
    // should pass.
    Err(VmError::Unimplemented("generator functions")) => Ok(()),
    Err(err) => Err(err),
  }
}
