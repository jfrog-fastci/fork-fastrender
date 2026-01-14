use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn private_fields_and_brand_checks_work_through_inheritance() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      class A {
        #x = 1;
        getX() { return this.#x; }
        static hasX(o) { return #x in o; }
      }

      class B extends A {
        #y = 2;
        getY() { return this.#y; }
        static hasY(o) { return #y in o; }
      }

      const a = new A();
      const b = new B();

      // Base private fields/methods must work on derived instances.
      let ok = true;
      ok = ok && b.getX() === 1 && b.getY() === 2;

      // Brand checks should succeed for derived instances when checking base private names.
      ok = ok && A.hasX(a) === true;
      ok = ok && A.hasX(b) === true;
      ok = ok && A.hasX({}) === false;

      // Derived private names are not present on base instances.
      ok = ok && B.hasY(b) === true;
      ok = ok && B.hasY(a) === false;
      ok = ok && B.hasY({}) === false;

      // Calling a method that uses a private name with a non-branded receiver should throw.
      let threw = false;
      try { A.prototype.getX.call({}); } catch (e) { threw = e instanceof TypeError; }

      ok && threw
    "#,
  )?;

  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn derived_field_initializers_can_call_super_methods_that_read_private_fields() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let value = rt.exec_script(
    r#"
      class A {
        #x = 3;
        getX() { return this.#x; }
      }

      class B extends A {
        y = super.getX();
      }

      (new B()).y === 3
    "#,
  )?;

  assert_eq!(value, Value::Bool(true));
  Ok(())
}

