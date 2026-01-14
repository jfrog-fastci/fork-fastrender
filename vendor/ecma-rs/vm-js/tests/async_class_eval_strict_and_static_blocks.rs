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
fn async_class_definition_is_strict_even_in_sloppy_outer_code() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = "";
      async function f() {
        try {
          class C {
            [(await Promise.resolve(0), unbound = 1, "m")]() {}
          }
          return "no";
        } catch (e) {
          return e.name;
        }
      }
      f().then(v => out = v);
    "#,
  )?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "ReferenceError");
  Ok(())
}

#[test]
fn async_class_evaluation_supports_static_blocks() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = "";
      async function f() {
        class C {
          [(await Promise.resolve("m"))]() { return 1; }
          static { out += "s"; }
        }
        return out;
      }
      f().then(v => out = v);
    "#,
  )?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "s");
  Ok(())
}

#[test]
fn await_in_async_class_static_block_is_allowed() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = "";
      async function f() {
        class C {
          static {
            out += "a";
            await Promise.resolve(0);
            out += "b";
          }
        }
        return out;
      }
      f().then(v => out = v);
    "#,
  )?;

  // Static block evaluation has started, but the `await` has not yet resumed.
  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "a");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "ab");
  Ok(())
}

#[test]
fn await_in_class_static_block_runs_as_async_script() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      var out = "";
      out += "a";
      class C {
        static {
          out += "b";
          await Promise.resolve(0);
          out += "c";
        }
      }
      out += "d";
    "#,
  )?;
  let Value::Object(promise_obj) = value else {
    return Err(VmError::InvariantViolation(
      "expected top-level await in script to return a promise object",
    ));
  };
  assert!(rt.heap.is_promise_object(promise_obj));

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "ab");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "abcd");
  Ok(())
}

#[test]
fn async_class_can_extend_awaited_expression_and_wires_prototypes() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = "";
      async function f() {
        class B {}
        class D extends (await Promise.resolve(B)) {}
        out += (Object.getPrototypeOf(D) === B ? "S" : "s");
        out += (Object.getPrototypeOf(D.prototype) === B.prototype ? "I" : "i");
        return out;
      }
      f().then(v => out = v);
    "#,
  )?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "SI");
  Ok(())
}

#[test]
fn async_class_heritage_must_be_constructor() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = "";
      async function f() {
        try {
          class D extends (await Promise.resolve(1)) {}
          return "no";
        } catch (e) {
          return e.message;
        }
      }
      f().then(v => out = v);
    "#,
  )?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(
    value_to_string(&rt, out),
    "Class extends value is not a constructor"
  );
  Ok(())
}

#[test]
fn async_class_explicit_constructor_body_supports_super_call() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = "";
      async function f() {
        class B { constructor(){ this.x = "b"; } }
        class D extends (await Promise.resolve(B)) {
          constructor() { super(); this.y = "d"; }
        }
        var d = new D();
        out += d.x + d.y;
        out += (Object.getPrototypeOf(D) === B ? "S" : "s");
        out += (Object.getPrototypeOf(D.prototype) === B.prototype ? "I" : "i");
        return out;
      }
      f().then(v => out = v);
    "#,
  )?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "bdSI");
  Ok(())
}

#[test]
fn async_class_can_extend_awaited_null_and_wires_null_prototype() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = "";
      async function f() {
        class D extends (await Promise.resolve(null)) {}
        out += (Object.getPrototypeOf(D) === Function.prototype ? "S" : "s");
        out += (Object.getPrototypeOf(D.prototype) === null ? "N" : "n");
        return out;
      }
      f().then(v => out = v);
    "#,
  )?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "SN");
  Ok(())
}

#[test]
fn async_class_heritage_evaluation_is_strict_even_in_sloppy_outer_code() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = "";
      async function f() {
        try {
          class C extends (await Promise.resolve(0), unbound = 1, null) {}
          return "no";
        } catch (e) {
          return e.name;
        }
      }
      f().then(v => out = v);
    "#,
  )?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "ReferenceError");
  Ok(())
}

#[test]
fn async_class_heritage_requires_object_or_null_super_prototype() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = "";
      async function f() {
        function B() {}
        B.prototype = 1;
        try {
          class D extends (await Promise.resolve(B)) {}
          return "no";
        } catch (e) {
          return e.message;
        }
      }
      f().then(v => out = v);
    "#,
  )?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(
    value_to_string(&rt, out),
    "Class extends value does not have a valid prototype property"
  );
  Ok(())
}

#[test]
fn async_class_can_extend_non_awaited_expression_when_other_parts_suspend() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = "";
      async function f() {
        class B {}
        class D extends B {
          // Force async class evaluation via an awaited computed key.
          [(await Promise.resolve("m"))]() {}
        }
        out += (Object.getPrototypeOf(D) === B ? "S" : "s");
        out += (Object.getPrototypeOf(D.prototype) === B.prototype ? "I" : "i");
        return out;
      }
      f().then(v => out = v);
    "#,
  )?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "SI");
  Ok(())
}

#[test]
fn async_class_heritage_self_reference_is_tdz_error() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = "";
      async function f() {
        try {
          // Force async class evaluation via an awaited computed key.
          class C extends C { [(await Promise.resolve("m"))]() {} }
          return "no";
        } catch (e) {
          return e.name;
        }
      }
      f().then(v => out = v);
    "#,
  )?;

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "ReferenceError");
  Ok(())
}

#[test]
fn script_await_in_class_heritage_runs_as_async_script() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = "";
      class B {}
      out += "a";
      class D extends (await Promise.resolve(B)) {}
      out += (Object.getPrototypeOf(D) === B ? "S" : "s");
      out += (Object.getPrototypeOf(D.prototype) === B.prototype ? "I" : "i");
    "#,
  )?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "a");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "aSI");
  Ok(())
}

#[test]
fn async_class_methods_support_super_property_access_in_instance_and_static() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = "";
      async function f() {
        class B {
          foo() { return "b"; }
          static bar() { return "B"; }
        }
        class D extends (await Promise.resolve(B)) {
          foo() { return super.foo() + "d"; }
          static bar() { return super.bar() + "D"; }
        }
        out += (new D()).foo();
        out += D.bar();
        return out;
      }
      f().then(v => out = v);
    "#,
  )?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "bdBD");
  Ok(())
}

#[test]
fn async_class_constructor_body_supports_super_property_access() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = "";
      async function f() {
        class B { foo() { return "b"; } }
        class D extends (await Promise.resolve(B)) {
          constructor() { super(); this.v = super.foo(); }
        }
        return (new D()).v;
      }
      f().then(v => out = v, e => out = e.name + ":" + e.message);
    "#,
  )?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "b");
  Ok(())
}

#[test]
fn async_class_constructor_body_supports_super_property_set() -> Result<(), VmError> {
  let mut rt = new_runtime();

  rt.exec_script(
    r#"
      var out = "";
      async function f() {
        class B {
          set foo(v) { this._foo = v; }
          get foo() { return this._foo; }
        }
        class D extends (await Promise.resolve(B)) {
          constructor() { super(); super.foo = "x"; }
        }
        return (new D()).foo;
      }
      f().then(v => out = v, e => out = e.name + ":" + e.message);
    "#,
  )?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let out = rt.exec_script("out")?;
  assert_eq!(value_to_string(&rt, out), "x");
  Ok(())
}
