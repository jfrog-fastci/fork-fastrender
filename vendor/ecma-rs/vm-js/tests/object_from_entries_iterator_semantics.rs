use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // `Object.fromEntries` exercises iterator protocol and property definition logic which can have
  // a moderately high peak memory footprint while compiling/executing the test scripts. Give the
  // runtime a small 2MiB heap budget so the tests fail only on semantic regressions.
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn object_from_entries_does_not_close_on_next_throw() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var returnCount = 0;
      var iterable = {};
      iterable[Symbol.iterator] = function() {
        return {
          next: function() { throw "next"; },
          return: function() { returnCount++; return {}; }
        };
      };
      var threw = false;
      try { Object.fromEntries(iterable); } catch (e) { threw = (e === "next"); }
      threw && returnCount === 0;
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn object_from_entries_does_not_close_on_next_returning_non_object() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var returnCount = 0;
      var iterable = {};
      iterable[Symbol.iterator] = function() {
        return {
          next: function() { return 1; },
          return: function() { returnCount++; return {}; }
        };
      };
      var threwTypeError = false;
      try { Object.fromEntries(iterable); } catch (e) { threwTypeError = e && e.name === "TypeError"; }
      threwTypeError && returnCount === 0;
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn object_from_entries_does_not_close_on_throwing_done_accessor() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var returnCount = 0;
      var iterable = {};
      iterable[Symbol.iterator] = function() {
        return {
          next: function() {
            return {
              get done() { throw "done"; },
              get value() { throw "should not access value"; }
            };
          },
          return: function() { returnCount++; return {}; }
        };
      };
      var threw = false;
      try { Object.fromEntries(iterable); } catch (e) { threw = (e === "done"); }
      threw && returnCount === 0;
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn object_from_entries_does_not_close_on_throwing_value_accessor() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var returnCount = 0;
      var iterable = {};
      iterable[Symbol.iterator] = function() {
        return {
          next: function() {
            return {
              done: false,
              get value() { throw "value"; }
            };
          },
          return: function() { returnCount++; return {}; }
        };
      };
      var threw = false;
      try { Object.fromEntries(iterable); } catch (e) { threw = (e === "value"); }
      threw && returnCount === 0;
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn object_from_entries_closes_on_null_entry() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var returnCount = 0;
      var iterable = {};
      iterable[Symbol.iterator] = function() {
        var advanced = false;
        return {
          next: function() {
            if (advanced) { return { done: true }; }
            advanced = true;
            return { done: false, value: null };
          },
          return: function() { returnCount++; return {}; }
        };
      };
      var threwTypeError = false;
      try { Object.fromEntries(iterable); } catch (e) { threwTypeError = e && e.name === "TypeError"; }
      threwTypeError && returnCount === 1;
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}
