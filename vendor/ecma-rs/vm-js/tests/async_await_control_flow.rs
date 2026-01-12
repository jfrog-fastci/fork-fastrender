use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_string(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn throw_await_in_throw_stmt_is_catchable() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        try {
          throw await Promise.resolve("boom");
        } catch (e) {
          out = e;
        }
      }
      f();
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "boom");
  Ok(())
}

#[test]
fn await_in_for_triple_init_and_update() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        for (let i = await Promise.resolve(0); i < 2; await Promise.resolve(++i)) {
          out += i;
        }
        out += "done";
      }
      f();
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "01done");
  Ok(())
}

#[test]
fn await_in_for_in_rhs_and_body() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        for (var k in await Promise.resolve({ a: 1, b: 2 })) {
          out += k;
          await 0;
        }
      }
      f();
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "ab");
  Ok(())
}

#[test]
fn await_in_for_of_rhs_and_body() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        for (var v of await Promise.resolve(["a", "b"])) {
          out += v;
          await 0;
        }
      }
      f();
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "ab");
  Ok(())
}

#[test]
fn await_in_do_while_body_and_test() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        var i = 0;
        do {
          out += i;
          i++;
          await 0;
        } while (await Promise.resolve(i < 2));
        out += "done";
      }
      f();
      out
    "#,
  )?;
  // The loop body runs once and suspends at the first `await 0`.
  assert_eq!(value_to_string(&rt, value), "0");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "01done");
  Ok(())
}

#[test]
fn await_in_switch_test_case_and_body() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        switch (await Promise.resolve(2)) {
          case await Promise.resolve(1):
            out = "bad";
            break;
          case await Promise.resolve(2):
            out = "ok";
            await 0;
            break;
          default:
            out = "bad2";
        }
        out += "done";
      }
      f();
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "okdone");
  Ok(())
}

#[test]
fn await_in_labelled_for_break_label() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      async function f() {
        outer: for (let i = 0; i < 3; i++) {
          await 0;
          out += i;
          break outer;
        }
        out += "done";
      }
      f();
      out
    "#,
  )?;
  assert_eq!(value_to_string(&rt, value), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, value), "0done");
  Ok(())
}
