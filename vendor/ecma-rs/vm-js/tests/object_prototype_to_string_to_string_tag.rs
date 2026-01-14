use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> Result<JsRuntime, VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap)
}

fn value_to_utf8(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap().get_string(s).unwrap().to_utf8_lossy()
}

#[test]
fn object_prototype_to_string_symbol_tag_generators_builtin() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let value = rt.exec_script("var genFn = function* () {};\nObject.prototype.toString.call(genFn)")?;
  assert_eq!(value_to_utf8(&rt, value), "[object GeneratorFunction]");

  let value = rt.exec_script("var gen = genFn();\nObject.prototype.toString.call(gen)")?;
  assert_eq!(value_to_utf8(&rt, value), "[object Generator]");

  let value = rt.exec_script("String(gen)")?;
  assert_eq!(value_to_utf8(&rt, value), "[object Generator]");

  let value = rt.exec_script("Object.getPrototypeOf(gen) === genFn.prototype")?;
  assert_eq!(value, Value::Bool(true));

  // Non-string @@toStringTag values must be ignored (fall back to the builtin tag).
  //
  // For generator objects, the builtin tag is `"Object"`; the `"Generator"` tag is supplied via
  // `%GeneratorPrototype%[@@toStringTag]`.
  let value = rt.exec_script(
    "Object.defineProperty(genFn.prototype, Symbol.toStringTag, { configurable: true, get() { return {}; } });\n\
     Object.prototype.toString.call(gen)",
  )?;
  assert_eq!(value_to_utf8(&rt, value), "[object Object]");

  let value = rt.exec_script("String(gen)")?;
  assert_eq!(value_to_utf8(&rt, value), "[object Object]");

  // Deleting the overridden @@toStringTag should fall back to %GeneratorPrototype%[@@toStringTag].
  let value = rt.exec_script(
    "delete genFn.prototype[Symbol.toStringTag];\n\
     Object.prototype.toString.call(gen)",
  )?;
  assert_eq!(value_to_utf8(&rt, value), "[object Generator]");

  Ok(())
}

#[test]
fn object_prototype_to_string_error_instance_to_string_tag_override() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  // `Error.prototype` is an ordinary object without [[ErrorData]], so the builtinTag is "Object".
  // Per ECMA-262, the spec does not define `Error.prototype[@@toStringTag]`, which would otherwise
  // prevent `err[Symbol.toStringTag] = ...` from creating an own property in strict mode.
  let value = rt.exec_script("Object.prototype.toString.call(Error.prototype)")?;
  assert_eq!(value_to_utf8(&rt, value), "[object Object]");

  // Instance overrides via assignment must work even in strict mode.
  let value = rt.exec_script(
    "(function () { 'use strict'; var err = new Error(); err[Symbol.toStringTag] = 'test262'; return Object.prototype.toString.call(err); })()",
  )?;
  assert_eq!(value_to_utf8(&rt, value), "[object test262]");

  Ok(())
}

#[test]
fn object_prototype_to_string_ignores_non_string_to_string_tag_for_symbol() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let value = rt.exec_script(
    "delete Symbol.prototype[Symbol.toStringTag];\n\
     Object.prototype.toString.call(Symbol('desc'))",
  )?;
  assert_eq!(value_to_utf8(&rt, value), "[object Object]");

  let value = rt.exec_script(
    "Object.defineProperty(Math, Symbol.toStringTag, {value: Symbol()});\n\
     Object.prototype.toString.call(Math)",
  )?;
  assert_eq!(value_to_utf8(&rt, value), "[object Object]");

  let value = rt.exec_script(
    "delete JSON[Symbol.toStringTag];\n\
     Object.prototype.toString.call(JSON)",
  )?;
  assert_eq!(value_to_utf8(&rt, value), "[object Object]");

  Ok(())
}

#[test]
fn object_prototype_to_string_ignores_non_string_to_string_tag_for_bigint() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let value = rt.exec_script(
    "(function () {\n\
       var custom1 = 0n;\n\
       var custom2 = Object(0n);\n\
       var proto = Object.getPrototypeOf(custom2);\n\
       Object.defineProperty(proto, Symbol.toStringTag, {value: undefined});\n\
       return Object.prototype.toString.call(custom1) + ',' + Object.prototype.toString.call(custom2);\n\
     })()",
  )?;
  assert_eq!(value_to_utf8(&rt, value), "[object Object],[object Object]");

  let value = rt.exec_script(
    "(function () {\n\
       var custom1 = 0n;\n\
       var custom2 = Object(0n);\n\
       var proto = Object.getPrototypeOf(custom2);\n\
       Object.defineProperty(proto, Symbol.toStringTag, {value: null});\n\
       return Object.prototype.toString.call(custom1) + ',' + Object.prototype.toString.call(custom2);\n\
     })()",
  )?;
  assert_eq!(value_to_utf8(&rt, value), "[object Object],[object Object]");

  Ok(())
}

#[test]
fn object_prototype_to_string_to_string_tag_overrides_primitive_builtin_tags() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let value = rt.exec_script(
    "Boolean.prototype[Symbol.toStringTag] = 'test262';\n\
     Number.prototype[Symbol.toStringTag] = 'test262';\n\
     String.prototype[Symbol.toStringTag] = 'test262';\n\
     Object.prototype.toString.call(Boolean.prototype) + ',' +\n\
     Object.prototype.toString.call(true) + ',' +\n\
     Object.prototype.toString.call(Number.prototype) + ',' +\n\
     Object.prototype.toString.call(0) + ',' +\n\
     Object.prototype.toString.call(String.prototype) + ',' +\n\
     Object.prototype.toString.call('')",
  )?;
  assert_eq!(
    value_to_utf8(&rt, value),
    "[object test262],[object test262],[object test262],[object test262],[object test262],[object test262]"
  );

  let value = rt.exec_script(
    "Object.defineProperty(Symbol.prototype, Symbol.toStringTag, { value: 'test262' });\n\
     Object.prototype.toString.call(Symbol.prototype)",
  )?;
  assert_eq!(value_to_utf8(&rt, value), "[object test262]");

  Ok(())
}
