use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> Result<JsRuntime, VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap)
}

#[test]
fn object_prototype_to_string_generator_falls_back_when_to_string_tag_deleted() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let value = rt.exec_script(
    r#"
      (function () {
        function* g() { yield 1; }
        const it = g();
        if (Object.prototype.toString.call(it) !== "[object Generator]") return false;

        // Generators can have several prototype levels between the generator instance and
        // %IteratorPrototype% (e.g. a per-function prototype object, plus an internal prototype
        // that stores the intrinsic "Generator" @@toStringTag).
        //
        // Delete @@toStringTag from the entire prototype chain so it's actually absent.
        let proto = Object.getPrototypeOf(it);
        while (proto !== null) {
          delete proto[Symbol.toStringTag];
          proto = Object.getPrototypeOf(proto);
        }
        if (it[Symbol.toStringTag] !== undefined) return false;
        // When @@toStringTag is absent, Object.prototype.toString must fall back to the
        // built-in tag ("Object").
        return Object.prototype.toString.call(it) === "[object Object]";
      })()
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn object_prototype_to_string_generator_falls_back_when_to_string_tag_non_string() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let value = rt.exec_script(
    r#"
      (function () {
        function* g() { yield 1; }
        const it = g();
        if (Object.prototype.toString.call(it) !== "[object Generator]") return false;

        const proto1 = Object.getPrototypeOf(it);
        Object.defineProperty(proto1, Symbol.toStringTag, {
          get: function () { return 1; },
          configurable: true
        });
        if (it[Symbol.toStringTag] !== 1) return false;
        // When @@toStringTag is present but not a string, Object.prototype.toString must fall back
        // to the built-in tag ("Object").
        return Object.prototype.toString.call(it) === "[object Object]";
      })()
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}
