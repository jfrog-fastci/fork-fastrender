use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn generator_function_constructor_creates_generator_functions() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var GeneratorFunction = Object.getPrototypeOf(function*(){}).constructor;

      var g1 = GeneratorFunction("a", "yield a");
      var g2 = new GeneratorFunction("yield 1");
      var g3 = GeneratorFunction("return 1");

      var ok =
        typeof GeneratorFunction === "function" &&
        typeof g1 === "function" &&
        typeof g2 === "function" &&
        typeof g3 === "function" &&
        Object.getPrototypeOf(g1) === GeneratorFunction.prototype &&
        Object.getPrototypeOf(g2) === GeneratorFunction.prototype &&
        Object.getPrototypeOf(g3) === GeneratorFunction.prototype &&
        Object.getPrototypeOf(g1.prototype) === GeneratorFunction.prototype.prototype &&
        Object.getPrototypeOf(g2.prototype) === GeneratorFunction.prototype.prototype &&
        Object.getPrototypeOf(g3.prototype) === GeneratorFunction.prototype.prototype &&
        !Object.prototype.hasOwnProperty.call(g1.prototype, "constructor") &&
        !Object.prototype.hasOwnProperty.call(g2.prototype, "constructor") &&
        !Object.prototype.hasOwnProperty.call(g3.prototype, "constructor") &&
        g1.name === "anonymous" &&
        g2.name === "anonymous" &&
        g3.name === "anonymous";

      ok;
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn generator_function_constructor_syntax_error_is_catchable() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
      var GeneratorFunction = Object.getPrototypeOf(function*(){}).constructor;
      var threw = false;
      try {
        GeneratorFunction("yield", "return yield");
      } catch (e) {
        threw = e && e.name === "SyntaxError";
      }
      threw;
    "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

