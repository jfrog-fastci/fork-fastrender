use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // These tests intentionally exercise Promise combinator + IteratorClose paths, which allocate
  // more intermediate objects than most tiny smoke tests. Use a slightly larger heap limit so the
  // tests fail only on semantic regressions, not on incidental heap pressure.
  let heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn object_from_entries_suppresses_iterator_close_throw() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var returnCount = 0;
      var iterable = {};
      iterable[Symbol.iterator] = function() {
        var advanced = false;
        return {
          next: function() {
            if (advanced) return { done: true };
            advanced = true;
            // Post-step error: entry is null.
            return { done: false, value: null };
          },
          return: function() {
            returnCount++;
            throw "close";
          }
        };
      };
      var threwTypeError = false;
      try {
        Object.fromEntries(iterable);
      } catch (e) {
        threwTypeError = e && e.name === "TypeError";
      }
      threwTypeError && returnCount === 1;
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn weak_map_constructor_suppresses_iterator_close_throw() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var returnCount = 0;
      var iterable = {};
      iterable[Symbol.iterator] = function() {
        return {
          next: function() {
            // Provide an entry object (array) so iteration succeeds and we reach `set`.
            return { done: false, value: [] };
          },
          return: function() {
            returnCount++;
            throw "close";
          }
        };
      };
      WeakMap.prototype.set = function() {
        throw "set";
      };
      var threw = false;
      try {
        new WeakMap(iterable);
      } catch (e) {
        threw = (e === "set");
      }
      threw && returnCount === 1;
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn weak_set_constructor_suppresses_iterator_close_throw() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var returnCount = 0;
      var iterable = {};
      iterable[Symbol.iterator] = function() {
        return {
          next: function() {
            return { done: false, value: {} };
          },
          return: function() {
            returnCount++;
            throw "close";
          }
        };
      };
      WeakSet.prototype.add = function() {
        throw "add";
      };
      var threw = false;
      try {
        new WeakSet(iterable);
      } catch (e) {
        threw = (e === "add");
      }
      threw && returnCount === 1;
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn promise_all_suppresses_iterator_close_throw_on_reject() -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt.exec_script(
    r#"
      var returnCount = 0;
      var caught = undefined;

      var iter = {};
      iter[Symbol.iterator] = function() {
        var advanced = false;
        return {
          next: function() {
            if (advanced) return { done: true };
            advanced = true;
            return { done: false, value: 1 };
          },
          return: function() {
            returnCount++;
            throw "close";
          }
        };
      };

      // Use a custom constructor so `promiseResolve` throws during PerformPromiseAll, which should
      // trigger IteratorClose(done=false). The close throw must be suppressed.
      function P(executor) { return new Promise(executor); }
      P.resolve = function() { throw "resolve"; };

      Promise.all.call(P, iter).catch(function(e) { caught = e; });
    "#,
  )?;

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script(r#"caught === "resolve" && returnCount === 1"#)?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn promise_all_does_not_close_iterator_when_next_throws() -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt.exec_script(
    r#"
      var returnCount = 0;
      var caught = undefined;

      var iter = {};
      iter[Symbol.iterator] = function() {
        return {
          next: function() { throw "next"; },
          return: function() { returnCount++; return {}; },
        };
      };

      Promise.all(iter).catch(function(e) { caught = e; });
    "#,
  )?;

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script(r#"caught === "next" && returnCount === 0"#)?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn promise_race_does_not_close_iterator_when_next_throws() -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt.exec_script(
    r#"
      var returnCount = 0;
      var caught = undefined;

      var iter = {};
      iter[Symbol.iterator] = function() {
        return {
          next: function() { throw "next"; },
          return: function() { returnCount++; return {}; },
        };
      };

      Promise.race(iter).catch(function(e) { caught = e; });
    "#,
  )?;

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script(r#"caught === "next" && returnCount === 0"#)?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn promise_all_settled_does_not_close_iterator_when_next_throws() -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt.exec_script(
    r#"
      var returnCount = 0;
      var caught = undefined;

      var iter = {};
      iter[Symbol.iterator] = function() {
        return {
          next: function() { throw "next"; },
          return: function() { returnCount++; return {}; },
        };
      };

      Promise.allSettled(iter).catch(function(e) { caught = e; });
    "#,
  )?;

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script(r#"caught === "next" && returnCount === 0"#)?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn promise_any_does_not_close_iterator_when_next_throws() -> Result<(), VmError> {
  let mut rt = new_runtime();
  rt.exec_script(
    r#"
      var returnCount = 0;
      var caught = undefined;

      var iter = {};
      iter[Symbol.iterator] = function() {
        return {
          next: function() { throw "next"; },
          return: function() { returnCount++; return {}; },
        };
      };

      Promise.any(iter).catch(function(e) { caught = e; });
    "#,
  )?;

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script(r#"caught === "next" && returnCount === 0"#)?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}
