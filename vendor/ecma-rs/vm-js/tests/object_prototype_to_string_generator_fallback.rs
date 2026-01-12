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

        const proto1 = Object.getPrototypeOf(it);
        const proto2 = Object.getPrototypeOf(proto1);

        // Engines differ on whether the generator instance inherits directly from
        // %GeneratorPrototype% or from a per-function prototype object that in turn inherits from
        // %GeneratorPrototype%. Delete from both to ensure @@toStringTag is actually absent.
        delete proto1[Symbol.toStringTag];
        if (proto2 !== null) {
          delete proto2[Symbol.toStringTag];
        }

        if (proto1[Symbol.toStringTag] !== undefined) return false;
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
        return Object.prototype.toString.call(it) === "[object Object]";
      })()
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}
