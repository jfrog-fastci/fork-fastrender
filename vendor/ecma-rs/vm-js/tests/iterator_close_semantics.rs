use vm_js::{Agent, Budget, HeapLimits, VmError, VmOptions};

fn new_agent() -> Agent {
  Agent::with_options(
    VmOptions::default(),
    HeapLimits::new(16 * 1024 * 1024, 16 * 1024 * 1024),
  )
  .expect("create agent")
}

#[test]
fn break_triggers_iterator_close_and_return_throw_overrides_break() -> Result<(), VmError> {
  let mut agent = new_agent();
  let value = agent.run_script(
    "iter_close_throw_overrides_break.js",
    r#"
      var it = { [Symbol.iterator]: function() {
        return {
          next: function() { return { value: 1, done: false }; },
          return: function() { throw 'close'; }
        };
      } };
      var out = 'no';
      try { for (var x of it) { break; } } catch (e) { out = e; }
      out;
    "#,
    Budget::unlimited(1),
    None,
  )?;
  assert_eq!(agent.value_to_error_string(value), "close");
  Ok(())
}

#[test]
fn break_throws_type_error_if_iterator_return_is_not_callable() -> Result<(), VmError> {
  let mut agent = new_agent();
  let value = agent.run_script(
    "iter_close_non_callable_return.js",
    r#"
      var it = { [Symbol.iterator]: function() {
        return {
          next: function() { return { value: 1, done: false }; },
          return: 1
        };
      } };
      var out = 'no';
      try { for (var x of it) { break; } } catch (e) { out = e.name; }
      out;
    "#,
    Budget::unlimited(1),
    None,
  )?;
  assert_eq!(agent.value_to_error_string(value), "TypeError");
  Ok(())
}

