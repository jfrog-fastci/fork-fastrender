use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

// Regression test for `super` property access after `super()` in derived constructors.
//
// Mirrors the intent of test262 `language/statements/class/super/in-constructor.js`, but also
// exercises receiver handling via built-in native accessors/methods.
const SOURCE: &str = r#"
  class Base {}

  class Derived extends Base {
    constructor() {
      super();
      this.foo = 1;

      // Super method call uses the derived instance as the receiver.
      this.hasFoo = super.hasOwnProperty('foo');
      this.hasFooComputed = super['hasOwnProperty']('foo');

      // Super property get uses the derived instance as the receiver (native accessor).
      this.protoMatches = super.__proto__ === Derived.prototype;
      this.protoMatchesComputed = super['__proto__'] === Derived.prototype;
    }
  }

  const d = new Derived();
  d.hasFoo && d.hasFooComputed && d.protoMatches && d.protoMatchesComputed;
"#;

#[test]
fn super_property_in_derived_constructor_ast() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let result = rt.exec_script(SOURCE)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn super_property_in_derived_constructor_hir() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let script = CompiledScript::compile_script(&mut rt.heap, "<inline>", SOURCE)?;
  let result = rt.exec_compiled_script(script)?;
  assert_eq!(result, Value::Bool(true));
  Ok(())
}

