use vm_js::{
  Heap, HeapLimits, JsRuntime, PropertyKey, PropertyKind, Value, Vm, VmError, VmOptions,
};

fn new_runtime() -> Result<JsRuntime, VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap)
}

fn assert_generator_function_shape(
  rt: &mut JsRuntime,
  func: vm_js::GcObject,
) -> Result<(), VmError> {
  let intr = *rt.realm().intrinsics();

  let mut scope = rt.heap.scope();
  scope.push_root(Value::Object(func))?;

  let constructor_s = scope.alloc_string("constructor")?;
  let prototype_s = scope.alloc_string("prototype")?;
  scope.push_root(Value::String(constructor_s))?;
  scope.push_root(Value::String(prototype_s))?;
  let constructor_key = PropertyKey::from_string(constructor_s);
  let prototype_key = PropertyKey::from_string(prototype_s);

  // Object.getPrototypeOf(g) === %GeneratorFunction.prototype%.
  assert_eq!(
    scope.heap().object_prototype(func)?,
    Some(intr.generator_function_prototype())
  );

  // g.constructor === %GeneratorFunction% (via %GeneratorFunction.prototype%.constructor).
  let desc = scope
    .heap()
    .get_own_property(intr.generator_function_prototype(), constructor_key)?
    .expect("%GeneratorFunction.prototype% should have an own `constructor` property");
  let PropertyKind::Data { value, .. } = desc.kind else {
    panic!("constructor should be a data property");
  };
  assert_eq!(value, Value::Object(intr.generator_function()));

  // g.prototype (per-function).
  let desc = scope
    .heap()
    .get_own_property(func, prototype_key)?
    .expect("generator function should have an own `prototype` property");
  assert!(!desc.enumerable);
  assert!(!desc.configurable);
  let PropertyKind::Data { value, writable } = desc.kind else {
    panic!("prototype should be a data property");
  };
  assert!(writable);
  let Value::Object(instance_proto) = value else {
    panic!("prototype value should be an object");
  };
  scope.push_root(Value::Object(instance_proto))?;

  // Object.getPrototypeOf(g.prototype) === %GeneratorPrototype%.
  assert_eq!(
    scope.heap().object_prototype(instance_proto)?,
    Some(intr.generator_prototype())
  );

  // g.prototype must not have an own "constructor" property.
  assert!(
    scope
      .heap()
      .get_own_property(instance_proto, constructor_key)?
      .is_none(),
    "generator function instance prototype should not have an own `constructor` property"
  );

  Ok(())
}

#[test]
fn generator_function_declaration_object_shape() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let g_value = rt.exec_script("function* g() {} g")?;
  let Value::Object(g) = g_value else {
    panic!("expected function object");
  };

  assert_generator_function_shape(&mut rt, g)?;

  let throws = rt.exec_script("try { new g(); false } catch (e) { true }")?;
  assert_eq!(throws, Value::Bool(true));

  Ok(())
}

#[test]
fn generator_function_expression_object_shape() -> Result<(), VmError> {
  let mut rt = new_runtime()?;

  let g_value = rt.exec_script("var g = function*() {}; g")?;
  let Value::Object(g) = g_value else {
    panic!("expected function object");
  };

  assert_generator_function_shape(&mut rt, g)?;

  let throws = rt.exec_script("try { new g(); false } catch (e) { true }")?;
  assert_eq!(throws, Value::Bool(true));

  Ok(())
}
