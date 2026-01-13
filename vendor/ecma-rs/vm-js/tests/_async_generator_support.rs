#![allow(dead_code)]

use vm_js::{JsRuntime, PropertyKey, Value, VmError};

const ASYNC_GENERATOR_UNIMPLEMENTED_MESSAGE: &str = "async generator functions";

pub fn is_unimplemented_async_generator_error(
  rt: &mut JsRuntime,
  err: &VmError,
) -> Result<bool, VmError> {
  match err {
    VmError::Unimplemented(msg) if msg.contains(ASYNC_GENERATOR_UNIMPLEMENTED_MESSAGE) => {
      return Ok(true)
    }
    _ => {}
  }

  let Some(thrown) = err.thrown_value() else {
    return Ok(false);
  };
  let Value::Object(err_obj) = thrown else {
    return Ok(false);
  };

  // Older versions of vm-js reported unsupported async generators by throwing a catchable
  // `SyntaxError("async generator functions")` object at runtime. Newer call sites may instead
  // coerce `VmError::Unimplemented` into a regular `Error("unimplemented: async generator functions")`.
  // Treat both as "not supported" so tests can land before the feature is fully implemented.
  let intr = rt.realm().intrinsics();
  let syntax_error_proto = intr.syntax_error_prototype();
  let error_proto = intr.error_prototype();
  let proto = rt.heap().object_prototype(err_obj)?;
  if proto != Some(syntax_error_proto) && proto != Some(error_proto) {
    return Ok(false);
  }

  let mut scope = rt.heap_mut().scope();
  scope.push_root(Value::Object(err_obj))?;

  let message_key = PropertyKey::from_string(scope.alloc_string("message")?);
  let Some(Value::String(message_s)) = scope
    .heap()
    .object_get_own_data_property_value(err_obj, &message_key)?
  else {
    return Ok(false);
  };

  Ok(scope
    .heap()
    .get_string(message_s)?
    .to_utf8_lossy()
    .contains(ASYNC_GENERATOR_UNIMPLEMENTED_MESSAGE))
}

/// Returns `true` if the runtime can *execute* async generator functions.
///
/// This intentionally probes execution (by calling `.next()`), not just parsing or function-object
/// creation, so integration tests skip cleanly until async generator machinery exists.
///
/// On success the probe also clears any queued microtasks so later assertions start from a clean
/// microtask queue.
pub fn supports_async_generators(rt: &mut JsRuntime) -> Result<bool, VmError> {
  // Probe:
  // - parse `async function*`
  // - execute it (create iterator)
  // - resume it via `await iter.next()`
  //
  // This is intentionally stricter than "does `.next()` return a Promise?" so tests remain skipped
  // during partial implementations (e.g. promise creation works but the resume job does not).
  let probe = r#"
    var __ag_support_ok = false;
    (async function () {
      try {
        async function* __ag_support() { yield 1; }
        var it = __ag_support();
        var r1 = await it.next();
        var r2 = await it.next();
        __ag_support_ok = !!(
          r1 && r1.value === 1 && r1.done === false &&
          r2 && r2.value === undefined && r2.done === true
        );
      } catch (e) {
        __ag_support_ok = false;
      }
    })();
  "#;

  match rt.exec_script(probe) {
    Ok(_) => {}
    Err(err) if is_unimplemented_async_generator_error(rt, &err)? => {
      rt.teardown_microtasks();
      return Ok(false);
    }
    // Older/newer parse frontends may represent `async function*` as a syntax error rather than an
    // explicit "unimplemented" error. Treat it as unsupported so async generator tests skip.
    Err(VmError::Syntax(_)) => {
      rt.teardown_microtasks();
      return Ok(false);
    }
    Err(err) => return Err(err),
  }

  let result = (|| {
    // The probe runs in an async IIFE, so we need a microtask checkpoint to drive it to completion.
    match rt.vm.perform_microtask_checkpoint(&mut rt.heap) {
      Ok(()) => {}
      Err(err) if is_unimplemented_async_generator_error(rt, &err)? => return Ok(false),
      Err(err) => return Err(err),
    };

    match rt.exec_script("__ag_support_ok")? {
      Value::Bool(b) => Ok(b),
      _ => Ok(false),
    }
  })();

  // Always discard any queued microtasks after the probe (including in the unsupported case) so we
  // don't leak jobs into later assertions.
  rt.teardown_microtasks();

  result
}
