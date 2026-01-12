use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn optional_chaining_short_circuits_chain_continuation() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var ok = true;
        try {
          ok = ok && (null?.a.b === undefined);
        } catch (e) {
          ok = false;
        }
        ok
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn optional_chaining_does_not_confuse_actual_undefined_with_short_circuit() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var threw = false;
        try {
          ({ a: undefined })?.a.b;
        } catch (e) {
          threw = e && e.name === "TypeError";
        }
        threw
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn optional_member_call_preserves_this_binding() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        ({ x: 1, f() { return this.x; } })?.f()
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Number(1.0));
}

#[test]
fn optional_chaining_skips_computed_keys_and_call_args() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var log = [];
        var o = null;
        o?.[log.push(1)];
        o?.f(log.push(2));
        log.length === 0
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn optional_chaining_parentheses_break_propagation() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var threw = false;
        try {
          (null?.a).b;
        } catch (e) {
          threw = e && e.name === "TypeError";
        }
        threw
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn async_optional_chaining_short_circuits_chain_continuation() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = null;
      async function f() {
        return (await Promise.resolve(null))?.a.b;
      }
      f().then(function (v) { out = (v === undefined); });
      out
    "#,
  )?;
  assert_eq!(value, Value::Null);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn async_optional_chaining_skips_awaited_computed_key() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var hits = 0;
      var out = null;
      async function sideEffect() { hits++; return "k"; }
      async function f() {
        return (await Promise.resolve(null))?.[await sideEffect()];
      }
      f().then(function (v) { out = (v === undefined); });
      out
    "#,
  )?;
  assert_eq!(value, Value::Null);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value, Value::Bool(true));
  let value = rt.exec_script("hits")?;
  assert_eq!(value, Value::Number(0.0));
  Ok(())
}

#[test]
fn async_optional_chaining_skips_awaited_call_args() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var hits = 0;
      var out = null;
      async function sideEffect() { hits++; return 123; }
      async function f() {
        var o = null;
        return o?.f(await sideEffect());
      }
      f().then(function (v) { out = (v === undefined); });
      out
    "#,
  )?;
  assert_eq!(value, Value::Null);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value, Value::Bool(true));
  let value = rt.exec_script("hits")?;
  assert_eq!(value, Value::Number(0.0));
  Ok(())
}

#[test]
fn async_optional_member_call_preserves_this_binding() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = null;
      async function f() {
        var o = { x: 1, m() { return this.x; } };
        return (await Promise.resolve(o))?.m();
      }
      f().then(function (v) { out = v; });
      out
    "#,
  )?;
  assert_eq!(value, Value::Null);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value, Value::Number(1.0));
  Ok(())
}

#[test]
fn async_optional_chaining_short_circuits_through_call_then_member() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = null;
      async function f() {
        return (await Promise.resolve(null))?.f().g;
      }
      f().then(function (v) { out = (v === undefined); });
      out
    "#,
  )?;
  assert_eq!(value, Value::Null);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

