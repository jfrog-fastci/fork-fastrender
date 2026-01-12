use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // WeakMap/WeakSet construction via AddEntriesFromIterable can allocate a non-trivial amount of
  // intermediate objects while compiling/executing these scripts; use a small 2MiB heap budget to
  // avoid spurious OOM failures.
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn weak_map_constructor_does_not_close_on_next_throw() -> Result<(), VmError> {
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
      try { new WeakMap(iterable); } catch (e) { threw = (e === "next"); }
      threw && returnCount === 0;
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn weak_map_constructor_closes_on_entry_not_object() -> Result<(), VmError> {
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
      try { new WeakMap(iterable); } catch (e) { threwTypeError = e && e.name === "TypeError"; }
      threwTypeError && returnCount === 1;
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn weak_set_constructor_does_not_close_on_next_throw() -> Result<(), VmError> {
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
      try { new WeakSet(iterable); } catch (e) { threw = (e === "next"); }
      threw && returnCount === 0;
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn weak_set_constructor_closes_on_add_throw_for_primitive_value() -> Result<(), VmError> {
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
            return { done: false, value: 1 };
          },
          return: function() { returnCount++; return {}; }
        };
      };
      var threwTypeError = false;
      try { new WeakSet(iterable); } catch (e) { threwTypeError = e && e.name === "TypeError"; }
      threwTypeError && returnCount === 1;
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}
