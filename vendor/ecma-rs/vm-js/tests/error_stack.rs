use vm_js::{Heap, HeapLimits, JsRuntime, PromiseState, PropertyKey, Value, Vm, VmError, VmOptions};

fn new_runtime() -> Result<JsRuntime, VmError> {
  let vm = Vm::new(VmOptions::default());
  // Async tests allocate Promise/job machinery; use a slightly larger heap than the minimal 1MiB
  // used by some unit tests to avoid spurious OOMs as builtin surface area grows.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap)
}

#[test]
fn error_stack_is_available_in_catch() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let v = rt.exec_script(
    "try { throw new Error('x'); } catch (e) { typeof e.stack === 'string' && e.stack.includes('Error') }",
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn error_stack_contains_multiple_frames() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let v = rt.exec_script(
    r#"
function outerCallFunction() { innerCallFunction(); }
function innerCallFunction() { throwerFunction(); }
function throwerFunction() { throw new Error("x"); }
try { outerCallFunction(); false } catch (e) {
  e.stack.includes("outerCallFunction")
    && e.stack.includes("innerCallFunction")
    && e.stack.includes("throwerFunction")
}
"#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn error_stack_for_async_implicit_throw_contains_frames() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  rt.exec_script(
    r#"
var captured = "";
async function f(){ await (null).x; }
f().catch(e => { captured = e.stack; });
"#,
  )?;
  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let v = rt.exec_script(
    r#"typeof captured === "string" && captured.includes("TypeError") && captured.includes("at ")"#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn error_stack_for_async_expr_body_implicit_throw_contains_frames() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  rt.exec_script(
    r#"
var captured = "";
var f = async () => await (null).x;
f().catch(e => { captured = e.stack; });
"#,
  )?;
  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let v = rt.exec_script(
    r#"typeof captured === "string" && captured.includes("TypeError") && captured.includes("at ")"#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn error_stack_for_async_expr_body_implicit_throw_after_await_contains_frames() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  rt.exec_script(
    r#"
var captured = "";
var f = async () => (await 0, (null).x);
f().catch(e => { captured = e.stack; });
"#,
  )?;
  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let v = rt.exec_script(
    r#"typeof captured === "string" && captured.includes("TypeError") && captured.includes("at ")"#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn error_stack_for_generator_implicit_throw_contains_frames() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let v = rt.exec_script(
    r#"
function* g(){ (null).x; }
try { g().next(); false } catch (e) {
  typeof e.stack === "string" && e.stack.includes("TypeError") && e.stack.includes("at ")
}
"#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn error_stack_is_bounded_for_long_function_names() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let long_name = "a".repeat(8 * 1024);
  let script = format!(
    r#"
function {name}() {{ throw new Error("x"); }}
try {{ {name}(); false }} catch (e) {{
  typeof e.stack === "string" && e.stack.length < 5000
}}
"#,
    name = long_name
  );
  let v = rt.exec_script(&script)?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn async_error_stack_is_bounded_for_long_function_names() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let long_name = "a".repeat(8 * 1024);
  let script = format!(
    r#"
var captured = "";
async function {name}() {{ await (null).x; }}
{name}().catch(e => {{ captured = e.stack; }});
"#,
    name = long_name
  );
  rt.exec_script(&script)?;
  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let v = rt.exec_script(r#"typeof captured === "string" && captured.length < 5000"#)?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn error_stack_for_async_await_revoked_proxy_promise_resolve_throw_contains_frames() -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  rt.exec_script(
    r#"
var captured = "";
async function f() {
  const { proxy, revoke } = Proxy.revocable({}, {});
  revoke();
  await proxy;
}
f().catch(e => { captured = e.stack; });
"#,
  )?;
  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let v = rt.exec_script(
    r#"typeof captured === "string"
      && captured.includes("TypeError")
      && captured.includes("revoked Proxy")
      && captured.includes("at ")"#,
  )?;
  assert_eq!(v, Value::Bool(true));
  Ok(())
}

#[test]
fn error_stack_for_top_level_await_revoked_proxy_promise_resolve_throw_has_error_stack(
) -> Result<(), VmError> {
  let mut rt = new_runtime()?;
  let result = rt.exec_script("const { proxy, revoke } = Proxy.revocable({}, {});\nrevoke();\nawait proxy;")?;
  let Value::Object(promise_obj) = result else {
    panic!("expected Promise object from top-level await script, got {result:?}");
  };
  assert!(rt.heap.is_promise_object(promise_obj));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap.promise_state(promise_obj)?, PromiseState::Rejected);
  let reason = rt
    .heap
    .promise_result(promise_obj)?
    .expect("rejected Promise should have a rejection reason");
  let Value::Object(reason_obj) = reason else {
    panic!("expected rejected promise reason to be an object, got {reason:?}");
  };

  let stack = {
    let mut scope = rt.heap.scope();
    scope.push_root(Value::Object(reason_obj))?;

    let stack_key_s = scope.alloc_string("stack")?;
    scope.push_root(Value::String(stack_key_s))?;
    let stack_key = PropertyKey::from_string(stack_key_s);

    let stack_v = scope
      .heap()
      .object_get_own_data_property_value(reason_obj, &stack_key)?
      .unwrap_or(Value::Undefined);
    let Value::String(stack_s) = stack_v else {
      panic!("expected rejection reason to have string own `stack`, got {stack_v:?}");
    };
    scope.heap().get_string(stack_s)?.to_utf8_lossy()
  };

  assert!(!stack.is_empty(), "expected non-empty stack string");
  assert!(
    stack.contains("TypeError") && stack.contains("revoked Proxy"),
    "expected stack string to contain error name/message, got {stack:?}"
  );
  assert!(
    stack.contains("at ") && stack.contains("<inline>:3:1"),
    "expected stack string to contain stack frames, got {stack:?}"
  );
  Ok(())
}
