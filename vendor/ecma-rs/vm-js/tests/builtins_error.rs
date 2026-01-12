use vm_js::{
  Heap, HeapLimits, JsRuntime, PropertyDescriptor, PropertyKey, PropertyKind, Value, Vm, VmError,
  VmOptions,
};

fn get_own_property<'a>(
  rt: &'a mut JsRuntime,
  obj: vm_js::GcObject,
  key: &str,
) -> Result<Option<PropertyDescriptor>, VmError> {
  let mut scope = rt.heap_mut().scope();
  scope.push_root(Value::Object(obj))?;
  let key_s = scope.alloc_string(key)?;
  let key = PropertyKey::from_string(key_s);
  scope.heap().object_get_own_property(obj, &key)
}

#[test]
fn error_prototype_to_string_on_null_or_undefined_throws_type_error() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // Ensure the exception is catchable from JS.
  let result =
    rt.exec_script(r#"try { Error.prototype.toString.call(undefined); "no"; } catch (e) { e.name }"#)?;
  let Value::String(s) = result else {
    panic!("expected string result, got {result:?}");
  };
  let actual = rt.heap_mut().get_string(s)?.to_utf8_lossy();
  assert_eq!(actual, "TypeError");
  Ok(())
}

#[test]
fn error_constructor_ignores_non_object_constructor_prototype() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // User code can mutate `Error.prototype` on the constructor (writable data property). This must
  // not cause construction to return `VmError::Unimplemented`.
  let value = rt.exec_script(r#"Error.prototype = undefined; new Error("x")"#)?;
  let Value::Object(obj) = value else {
    panic!("expected error object, got {value:?}");
  };

  let error_prototype = rt.realm().intrinsics().error_prototype();
  let mut scope = rt.heap_mut().scope();
  scope.push_root(Value::Object(obj))?;
  assert_eq!(scope.heap().object_prototype(obj)?, Some(error_prototype));
  Ok(())
}

#[test]
fn aggregate_error_converts_iterable_to_errors_array() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let result = rt.exec_script(
    r#"
    const iterable = {};
    let i = 0;
    iterable[Symbol.iterator] = function() {
      return {
        next: function() {
          i++;
          if (i === 1) return { value: 1, done: false };
          if (i === 2) return { value: 2, done: false };
          return { value: undefined, done: true };
        }
      };
    };
    const e = new AggregateError(iterable, "m");
    Array.isArray(e.errors) && e.errors.length === 2 && e.errors[0] === 1 && e.errors[1] === 2;
    "#,
  )?;

  assert_eq!(result, Value::Bool(true));
  Ok(())
}

#[test]
fn error_cause_option_is_installed() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let value = rt.exec_script(r#"new Error("m", { cause: 123 })"#)?;
  let Value::Object(obj) = value else {
    panic!("expected error object, got {value:?}");
  };

  let desc = get_own_property(&mut rt, obj, "cause")?.expect("expected own 'cause' property");
  assert!(!desc.enumerable, "cause should be non-enumerable");
  assert!(desc.configurable, "cause should be configurable");
  let PropertyKind::Data { value, writable } = desc.kind else {
    panic!("cause should be a data property");
  };
  assert!(writable, "cause should be writable");
  assert_eq!(value, Value::Number(123.0));
  Ok(())
}
