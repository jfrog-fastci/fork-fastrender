use crate::class_fields::class_constructor_body;
use crate::function::{CallHandler, FunctionData};
use crate::{
  CompiledScript, GcObject, Heap, HeapLimits, JsRuntime, PropertyKey, Value, Vm, VmError, VmOptions,
};

fn new_runtime() -> Result<JsRuntime, VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap)
}

fn compile_and_get_class_ctor(rt: &mut JsRuntime, source: &str) -> Result<GcObject, VmError> {
  let script = CompiledScript::compile_script(&mut rt.heap, "<inline>", source)?;
  assert!(
    !script.requires_ast_fallback,
    "expected script to run via compiled HIR path (requires_ast_fallback=false)"
  );
  let value = rt.exec_compiled_script(script)?;
  let Value::Object(class_ctor) = value else {
    panic!("expected script to evaluate to a class constructor object, got {value:?}");
  };
  Ok(class_ctor)
}

fn get_compiled_ctor_body_function(
  rt: &mut JsRuntime,
  class_ctor: GcObject,
) -> Result<GcObject, VmError> {
  let mut scope = rt.heap.scope();

  // Root the constructor object across native-slot and function metadata lookups.
  scope.push_root(Value::Object(class_ctor))?;

  // Compiled class evaluation stores a native wrapper function in the class constructor's
  // `[[ConstructorBody]]` slot. That wrapper's first native slot holds the compiled user function
  // for the actual constructor body.
  let wrapper = class_constructor_body(&scope, class_ctor)?
    .expect("expected compiled class constructor to have a constructor body");
  scope.push_root(Value::Object(wrapper))?;

  let wrapper_slots = scope.heap().get_function_native_slots(wrapper)?;
  let Some(Value::Object(body_func)) = wrapper_slots.first().copied() else {
    panic!("expected constructor body wrapper to store inner body function");
  };

  // Sanity-check that we found a compiled user function and that it is annotated as a class
  // constructor body for the given class constructor object.
  let CallHandler::User(func_ref) = scope.heap().get_function_call_handler(body_func)? else {
    panic!("expected constructor body to be a compiled user function");
  };
  assert!(
    func_ref.ast_fallback.is_none(),
    "expected constructor body to execute via compiled HIR path (ast_fallback=None)"
  );

  match scope.heap().get_function_data(body_func)? {
    FunctionData::ClassConstructorBody { class_constructor } => {
      assert_eq!(
        class_constructor, class_ctor,
        "constructor body should reference its containing class constructor"
      );
    }
    other => {
      panic!("expected FunctionData::ClassConstructorBody, got {other:?}");
    }
  }

  Ok(body_func)
}

#[test]
fn hir_constructing_class_ctor_body_directly_supports_super_in_arrow() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let class_ctor = compile_and_get_class_ctor(
    &mut rt,
    r#"
      class B { constructor(){ this.x = 1; } }
      class D extends B {
        constructor(){
          (() => super())();
        }
      }
      D
    "#,
  )?;
  let body_func = get_compiled_ctor_body_function(&mut rt, class_ctor)?;

  let mut scope = rt.heap.scope();
  scope.push_root(Value::Object(class_ctor))?;
  scope.push_root(Value::Object(body_func))?;

  // Construct the compiled constructor body function directly, forwarding the containing class
  // constructor as `new.target` (matching how `class_constructor_construct` delegates).
  let result = rt.vm.construct_without_host(
    &mut scope,
    Value::Object(body_func),
    &[],
    Value::Object(class_ctor),
  )?;
  let Value::Object(instance) = result else {
    panic!("expected constructor body to produce an object, got {result:?}");
  };

  // Verify base-class side effects ran (i.e. the `super()` call executed).
  scope.push_root(Value::Object(instance))?;
  let key_s = scope.alloc_string("x")?;
  scope.push_root(Value::String(key_s))?;
  let key = PropertyKey::from_string(key_s);
  let x_val = scope
    .heap()
    .object_get_own_data_property_value(instance, &key)?
    .unwrap_or(Value::Undefined);
  assert_eq!(x_val, Value::Number(1.0));
  Ok(())
}

#[test]
fn hir_constructing_class_ctor_body_directly_supports_super_in_direct_eval() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let class_ctor = compile_and_get_class_ctor(
    &mut rt,
    r#"
      class B { constructor(){ this.x = 1; } }
      class D extends B {
        constructor(){
          eval('super()');
        }
      }
      D
    "#,
  )?;
  let body_func = get_compiled_ctor_body_function(&mut rt, class_ctor)?;

  let mut scope = rt.heap.scope();
  scope.push_root(Value::Object(class_ctor))?;
  scope.push_root(Value::Object(body_func))?;

  let result = rt.vm.construct_without_host(
    &mut scope,
    Value::Object(body_func),
    &[],
    Value::Object(class_ctor),
  )?;
  let Value::Object(instance) = result else {
    panic!("expected constructor body to produce an object, got {result:?}");
  };

  scope.push_root(Value::Object(instance))?;
  let key_s = scope.alloc_string("x")?;
  scope.push_root(Value::String(key_s))?;
  let key = PropertyKey::from_string(key_s);
  let x_val = scope
    .heap()
    .object_get_own_data_property_value(instance, &key)?
    .unwrap_or(Value::Undefined);
  assert_eq!(x_val, Value::Number(1.0));
  Ok(())
}

