use vm_js::{CompiledScript, Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn exec_compiled(rt: &mut JsRuntime, source: &str) -> Result<Value, VmError> {
  let script = CompiledScript::compile_script(rt.heap_mut(), "<method_definition_super_property>", source)?;
  rt.exec_compiled_script(script)
}

// Mirrors test262:
// - `language/expressions/object/method-definition/name-super-prop-param.js`
// - `language/expressions/object/method-definition/name-super-prop-body.js`
// - `language/expressions/object/method-definition/generator-super-prop-param.js`
// - `language/expressions/object/method-definition/generator-super-prop-body.js`
const OBJECT_LITERAL_METHOD_DEFINITION_SUPER_PROPERTY: &str = r#"
  var obj1 = {
    method(x = super.toString) {
      return x;
    }
  };
  obj1.toString = null;
  var ok1 = obj1.method() === Object.prototype.toString;

  var obj2 = {
    method() {
      return super.toString;
    }
  };
  obj2.toString = null;
  var ok2 = obj2.method() === Object.prototype.toString;

  var obj3 = {
    *foo(a = super.toString) {
      return a;
    }
  };
  obj3.toString = null;
  var ok3 = obj3.foo().next().value === Object.prototype.toString;

  var obj4 = {
    *foo() {
      return super.toString;
    }
  };
  obj4.toString = null;
  var ok4 = obj4.foo().next().value === Object.prototype.toString;

  ok1 && ok2 && ok3 && ok4;
"#;

#[test]
fn object_literal_method_definition_super_property_ast() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(OBJECT_LITERAL_METHOD_DEFINITION_SUPER_PROPERTY)?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn object_literal_method_definition_super_property_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(&mut rt, OBJECT_LITERAL_METHOD_DEFINITION_SUPER_PROPERTY)?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

// Mirrors test262 `language/statements/class/syntax/class-body-method-definition-super-property.js`.
const CLASS_BODY_METHOD_DEFINITION_SUPER_PROPERTY: &str = r#"
  class A {
    constructor() {
      super.toString();
    }
    dontDoThis() {
      super.makeBugs = 1;
    }
  }

  var a = new A();
  a.dontDoThis();

  typeof A === "function" && a.makeBugs === 1;
"#;

#[test]
fn class_body_method_definition_super_property_ast() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(CLASS_BODY_METHOD_DEFINITION_SUPER_PROPERTY)?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn class_body_method_definition_super_property_compiled() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = exec_compiled(&mut rt, CLASS_BODY_METHOD_DEFINITION_SUPER_PROPERTY)?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

