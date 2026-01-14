use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_number(value: Value) -> f64 {
  let Value::Number(n) = value else {
    panic!("expected number, got {value:?}");
  };
  n
}

#[test]
fn async_class_evaluation_supports_private_methods() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = 0;
      async function f() {
        class C {
          static #m() { return 1; }
          static [(await Promise.resolve("call"))]() { return this.#m(); }
        }
        return C.call();
      }
      f().then(v => out = v);
    "#,
  )?;

  assert_eq!(value_to_number(rt.exec_script("out")?), 0.0);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(value_to_number(rt.exec_script("out")?), 1.0);
  Ok(())
}

#[test]
fn async_class_evaluation_supports_private_accessors() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = 0;
      var side = 0;
      async function f() {
        class C {
          static get #x() { return 2; }
          static set #x(v) { side = v; }
          static [(await Promise.resolve("call"))]() {
            this.#x = 5;
            return this.#x;
          }
        }
        return C.call();
      }
      f().then(v => out = v);
    "#,
  )?;

  assert_eq!(value_to_number(rt.exec_script("out")?), 0.0);
  assert_eq!(value_to_number(rt.exec_script("side")?), 0.0);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(value_to_number(rt.exec_script("out")?), 2.0);
  assert_eq!(value_to_number(rt.exec_script("side")?), 5.0);
  Ok(())
}

#[test]
fn async_class_evaluation_supports_private_methods_when_heritage_suspends() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = 0;
      async function f() {
        class B {}
        class C extends (await Promise.resolve(B)) {
          static #m() { return 3; }
          static call() { return this.#m(); }
        }
        return C.call();
      }
      f().then(v => out = v);
    "#,
  )?;

  assert_eq!(value_to_number(rt.exec_script("out")?), 0.0);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(value_to_number(rt.exec_script("out")?), 3.0);
  Ok(())
}

#[test]
fn class_static_block_can_access_private_methods() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = 0;
      class C {
        static #m() { return 1; }
        static {
          out = out * 10 + this.#m();
          out = out * 10 + this.#m();
        }
      }
    "#,
  )?;

  assert_eq!(value_to_number(rt.exec_script("out")?), 11.0);
  Ok(())
}

#[test]
fn async_class_evaluation_supports_private_instance_methods() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = 0;
      async function f() {
        class C {
          #m() { return 4; }
          [(await Promise.resolve("call"))]() { return this.#m(); }
        }
        return (new C()).call();
      }
      f().then(v => out = v);
    "#,
  )?;

  assert_eq!(value_to_number(rt.exec_script("out")?), 0.0);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(value_to_number(rt.exec_script("out")?), 4.0);
  Ok(())
}

#[test]
fn async_class_evaluation_supports_private_instance_accessors() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = 0;
      var side = 0;
      async function f() {
        class C {
          get #x() { return 2; }
          set #x(v) { side = v; }
          [(await Promise.resolve("call"))]() {
            this.#x = 5;
            return this.#x;
          }
        }
        return (new C()).call();
      }
      f().then(v => out = v);
    "#,
  )?;

  assert_eq!(value_to_number(rt.exec_script("out")?), 0.0);
  assert_eq!(value_to_number(rt.exec_script("side")?), 0.0);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(value_to_number(rt.exec_script("out")?), 2.0);
  assert_eq!(value_to_number(rt.exec_script("side")?), 5.0);
  Ok(())
}
