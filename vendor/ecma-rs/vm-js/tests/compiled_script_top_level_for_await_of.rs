use vm_js::{
  CompiledScript, Heap, HeapLimits, JsRuntime, PromiseState, PropertyDescriptor, PropertyKey,
  PropertyKind, Value, Vm, VmError, VmOptions,
};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Promise + async iterator machinery needs a bit of heap headroom.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn value_to_utf8(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap.get_string(s).unwrap().to_utf8_lossy()
}

fn define_global(rt: &mut JsRuntime, name: &str, value: Value) -> Result<(), VmError> {
  let global = rt.realm().global_object();
  let mut scope = rt.heap_mut().scope();
  scope.push_root(Value::Object(global))?;
  scope.push_root(value)?;

  let key_s = scope.alloc_string(name)?;
  scope.push_root(Value::String(key_s))?;
  let key = PropertyKey::from_string(key_s);
  scope.define_property(
    global,
    key,
    PropertyDescriptor {
      enumerable: true,
      configurable: true,
      kind: PropertyKind::Data {
        value,
        writable: true,
      },
    },
  )?;
  Ok(())
}

#[test]
fn compiled_script_top_level_for_await_of_executes_via_hir_and_resumes_in_microtasks(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var actual = [];
      for await (const x of [Promise.resolve("a"), "b"]) {
        actual.push(x);
      }
      actual.push("done");
    "#,
  )?;
  assert!(
    !script.requires_ast_fallback,
    "simple top-level for-await-of loops should execute via the compiled (HIR) async script path"
  );

  let result = rt.exec_compiled_script(script)?;
  let Value::Object(promise_obj) = result else {
    panic!("expected Promise object, got {result:?}");
  };
  assert!(
    rt.heap().is_promise_object(promise_obj),
    "expected Promise return value from async classic script"
  );
  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Pending);

  let before = rt.exec_script("JSON.stringify(actual)")?;
  assert_eq!(value_to_utf8(&rt, before), r#"[]"#);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);
  let after = rt.exec_script("JSON.stringify(actual)")?;
  assert_eq!(value_to_utf8(&rt, after), r#"["a","b","done"]"#);

  Ok(())
}

#[test]
fn compiled_script_top_level_for_await_of_allows_direct_await_in_rhs() -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var actual = [];
      var iterable = [Promise.resolve("a"), "b"];
      for await (const x of await Promise.resolve(iterable)) {
        actual.push(x);
      }
      actual.push("done");
    "#,
  )?;
  assert!(script.contains_top_level_await);
  assert!(
    !script.top_level_await_requires_ast_fallback,
    "top-level for-await-of should support a direct `await <expr>` RHS in the HIR async script executor"
  );
  assert!(
    !script.requires_ast_fallback,
    "top-level for-await-of with a direct await RHS should execute via the compiled (HIR) async script path"
  );

  let result = rt.exec_compiled_script(script)?;
  let Value::Object(promise_obj) = result else {
    panic!("expected Promise object, got {result:?}");
  };
  assert!(
    rt.heap().is_promise_object(promise_obj),
    "expected Promise return value from async classic script"
  );
  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Pending);

  let before = rt.exec_script("JSON.stringify(actual)")?;
  assert_eq!(value_to_utf8(&rt, before), r#"[]"#);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);
  let after = rt.exec_script("JSON.stringify(actual)")?;
  assert_eq!(value_to_utf8(&rt, after), r#"["a","b","done"]"#);

  Ok(())
}

#[test]
fn compiled_script_top_level_for_await_of_throw_suppresses_iterator_return_rejection(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var returnCalls = 0;
      const iterable = {};
      iterable[Symbol.asyncIterator] = function () {
        return {
          next() {
            return Promise.resolve({ value: 1, done: false });
          },
          return() {
            returnCalls++;
            return Promise.reject("close");
          },
        };
      };

      for await (const x of iterable) {
        throw "body";
      }
    "#,
  )?;
  assert!(
    !script.requires_ast_fallback,
    "top-level for-await-of loops with synchronous bodies should execute via the compiled (HIR) async script path"
  );

  let result = rt.exec_compiled_script(script)?;
  let Value::Object(promise_obj) = result else {
    panic!("expected Promise object, got {result:?}");
  };
  assert!(rt.heap().is_promise_object(promise_obj));
  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Pending);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  // Per ECMA-262 `AsyncIteratorClose`, errors from awaiting `iterator.return()` are suppressed for
  // throw completions (the original throw must be preserved).
  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Rejected);
  let reason = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("rejected promise should have a reason");
  assert_eq!(value_to_utf8(&rt, reason), "body");

  let return_calls = rt.exec_script("returnCalls")?;
  assert_eq!(return_calls, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_script_top_level_for_await_of_async_iterator_close_only_observes_promise_constructor_once(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var ctorCalls = 0;
      var returnCalls = 0;

      const closePromise = Promise.resolve({});
      Object.defineProperty(closePromise, "constructor", {
        get() {
          ctorCalls++;
          return Promise;
        },
      });

      const iterable = {};
      iterable[Symbol.asyncIterator] = function () {
        return {
          next() {
            return Promise.resolve({ value: 1, done: false });
          },
          return() {
            returnCalls++;
            return closePromise;
          },
        };
      };

      for await (const x of iterable) {
        break;
      }
    "#,
  )?;
  assert!(
    !script.requires_ast_fallback,
    "top-level for-await-of loops with synchronous bodies should execute via the compiled (HIR) async script path"
  );

  let result = rt.exec_compiled_script(script)?;
  let Value::Object(promise_obj) = result else {
    panic!("expected Promise object, got {result:?}");
  };
  assert!(rt.heap().is_promise_object(promise_obj));
  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Pending);

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);

  // AsyncIteratorClose performs PromiseResolve on the return result exactly once; the outer
  // suspension machinery must not PromiseResolve it again (or `promise.constructor` would be
  // observed twice).
  let ctor_calls = rt.exec_script("ctorCalls")?;
  assert_eq!(ctor_calls, Value::Number(1.0));

  let return_calls = rt.exec_script("returnCalls")?;
  assert_eq!(return_calls, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_script_top_level_labeled_for_await_of_break_label_executes_and_closes_iterator() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      var actual = "";
      var returnCalls = 0;

      const iterable = {};
      iterable[Symbol.asyncIterator] = function () {
        return {
          i: 0,
          next() {
            if (this.i++ === 0) return Promise.resolve({ value: "a", done: false });
            return Promise.resolve({ value: "b", done: false });
          },
          return() {
            returnCalls++;
            return Promise.resolve({ done: true });
          },
        };
      };

      outer: for await (const x of iterable) {
        actual += x;
        break outer;
      }
      actual += "done";
    "#,
  )?;
  assert!(
    !script.requires_ast_fallback,
    "labeled top-level for-await-of loops with synchronous bodies should execute via the compiled (HIR) async script path"
  );

  let result = rt.exec_compiled_script(script)?;
  let Value::Object(promise_obj) = result else {
    panic!("expected Promise object, got {result:?}");
  };
  assert!(rt.heap().is_promise_object(promise_obj));

  let before = rt.exec_script("actual")?;
  assert_eq!(value_to_utf8(&rt, before), "");
  let before_calls = rt.exec_script("returnCalls")?;
  assert_eq!(before_calls, Value::Number(0.0));

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Fulfilled);
  let after = rt.exec_script("actual")?;
  assert_eq!(value_to_utf8(&rt, after), "adone");
  let return_calls = rt.exec_script("returnCalls")?;
  assert_eq!(return_calls, Value::Number(1.0));
  Ok(())
}

#[test]
fn compiled_script_top_level_for_await_of_rhs_type_error_rejects_promise_with_stack(
) -> Result<(), VmError> {
  let mut rt = new_runtime();

  // Trigger a TypeError during evaluation of the RHS expression (before the iterator is acquired).
  // This must reject the async classic-script completion promise with a *real* Error object (not
  // surface as an internal invariant violation).
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "test.js",
    r#"
      for await (const x of (null).prop) {}
    "#,
  )?;
  assert!(
    !script.requires_ast_fallback,
    "simple top-level for-await-of loops should execute via the compiled (HIR) async script path"
  );

  let result = rt.exec_compiled_script(script)?;
  let Value::Object(promise_obj) = result else {
    panic!("expected Promise object, got {result:?}");
  };
  assert!(rt.heap().is_promise_object(promise_obj));

  // Rejecting the completion promise can happen synchronously (before the first await suspension),
  // but ensure all promise jobs have run before asserting on the settled state.
  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.heap().promise_state(promise_obj)?, PromiseState::Rejected);
  let reason = rt
    .heap()
    .promise_result(promise_obj)?
    .expect("rejected promise should have a reason");

  define_global(&mut rt, "__err", reason)?;
  let has_stack = rt.exec_script(
    r#"typeof __err.stack === "string" && __err.stack.includes("TypeError") && __err.stack.includes("at ")"#,
  )?;
  assert_eq!(has_stack, Value::Bool(true));
  Ok(())
}
