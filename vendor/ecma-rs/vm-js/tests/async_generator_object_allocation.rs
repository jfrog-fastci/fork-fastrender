use vm_js::{Heap, HeapLimits, JsRuntime, PropertyKey, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).expect("create runtime")
}

fn is_unimplemented_async_generator_error(rt: &mut JsRuntime, err: &VmError) -> Result<bool, VmError> {
  match err {
    VmError::Unimplemented(msg) if msg.contains("async generator functions") => return Ok(true),
    _ => {}
  }

  let Some(thrown) = err.thrown_value() else {
    return Ok(false);
  };
  let Value::Object(err_obj) = thrown else {
    return Ok(false);
  };

  // vm-js historically surfaced async generator support as a throwable SyntaxError (feature-detectable
  // via try/catch) so tests can land before full semantics are implemented.
  let syntax_error_proto = rt.realm().intrinsics().syntax_error_prototype();
  if rt.heap().object_prototype(err_obj)? != Some(syntax_error_proto) {
    return Ok(false);
  }

  let mut scope = rt.heap_mut().scope();
  scope.push_root(Value::Object(err_obj))?;

  let message_key = PropertyKey::from_string(scope.alloc_string("message")?);
  let Some(Value::String(message_s)) =
    scope.heap().object_get_own_data_property_value(err_obj, &message_key)?
  else {
    return Ok(false);
  };
  let message = scope.heap().get_string(message_s)?.to_utf8_lossy();
  Ok(message == "async generator functions")
}

fn feature_detect_async_generators(rt: &mut JsRuntime) -> Result<bool, VmError> {
  // Async generator syntax is supported, but full semantics (including allocating an async generator
  // object on invocation) may not be implemented yet. Feature-detect by actually calling an async
  // generator function.
  match rt.exec_script(
    r#"
      (() => {
        async function* __ag_support() {}
        __ag_support();
        return true;
      })()
    "#,
  ) {
    Ok(_) => Ok(true),
    Err(err) if is_unimplemented_async_generator_error(rt, &err)? => Ok(false),
    Err(err) => Err(err),
  }
}

#[test]
fn async_generator_function_call_allocates_async_generator_object_with_correct_prototype(
) -> Result<(), VmError> {
  let mut rt = new_runtime();
  if !feature_detect_async_generators(&mut rt)? {
    return Ok(());
  }
  
  // Basic async generator allocation + prototype selection.
  let value = match rt.exec_script(
    r#"
async function* g() {}
let ok = true;

const gen1 = g();
ok = ok && typeof gen1 === "object";
ok = ok && Object.getPrototypeOf(gen1) === g.prototype;

g.prototype = null;
const gen2 = g();
ok = ok && Object.getPrototypeOf(gen2) === Object.getPrototypeOf(g).prototype;
ok;
"#,
  ) {
    Ok(value) => value,
    Err(err) if is_unimplemented_async_generator_error(&mut rt, &err)? => return Ok(()),
    Err(err) => return Err(err),
  };
  assert_eq!(value, Value::Bool(true));

  // `%AsyncGeneratorPrototype%.next` should recognize the async generator object and return a
  // Promise resolving to an iterator result object (rather than throwing a TypeError for an
  // incompatible receiver).
  let value = match rt.exec_script(
    r#"
async function* g() {}
const gen = g();
var out = "";
const p = Object.getPrototypeOf(g).prototype.next.call(gen);
p.then(r => { out = String(r.done) + ":" + String(r.value); });
out
"#,
  ) {
    Ok(value) => value,
    Err(err) if is_unimplemented_async_generator_error(&mut rt, &err)? => return Ok(()),
    Err(err) => return Err(err),
  };
  let Value::String(s) = value else {
    panic!("expected string result, got {value:?}");
  };
  assert_eq!(rt.heap.get_string(s)?.to_utf8_lossy(), "");

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let value = rt.exec_script("out")?;
  let Value::String(s) = value else {
    panic!("expected string result, got {value:?}");
  };
  assert_eq!(rt.heap.get_string(s)?.to_utf8_lossy(), "true:undefined");

  Ok(())
}
