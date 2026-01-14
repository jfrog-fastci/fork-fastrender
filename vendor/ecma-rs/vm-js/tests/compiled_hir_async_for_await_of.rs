use std::sync::Arc;
use vm_js::{
  CompiledFunctionRef, CompiledScript, Heap, HeapLimits, PromiseState, PropertyKey, Value, Vm, VmError,
  VmOptions,
};

fn find_function_body(script: &Arc<CompiledScript>, name: &str) -> hir_js::BodyId {
  let hir = script.hir.as_ref();
  for def in hir.defs.iter() {
    let Some(body_id) = def.body else {
      continue;
    };
    let Some(body) = hir.body(body_id) else {
      continue;
    };
    if body.kind != hir_js::BodyKind::Function {
      continue;
    }
    let def_name = hir.names.resolve(def.name).unwrap_or("");
    if def_name == name {
      return body_id;
    }
  }
  panic!("function body not found for name={name:?}");
}

#[test]
fn compiled_async_for_await_of_closure_capture_per_iteration_env() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "compiled_async_for_await_of_closure_capture.js",
    r#"
      async function f() {
        const fs = [];
        const iterable = {
          [Symbol.asyncIterator]() {
            let i = 0;
            return {
              next() {
                if (i < 3) {
                  i = i + 1;
                  return Promise.resolve({ value: i, done: false });
                }
                return Promise.resolve({ done: true });
              },
            };
          }
        };

        for await (let x of iterable) {
          fs.push(() => x);
        }
        return fs[0]() + fs[1]() + fs[2]();
      }
    "#,
  )?;
  let f_body = find_function_body(&script, "f");

  let mut vm = Vm::new(VmOptions::default());
  let mut realm = vm_js::Realm::new(&mut vm, &mut heap)?;

  let promise_root = {
    let promise_root_res: Result<_, VmError> = {
      let mut scope = heap.scope();
      (|| {
        let name = scope.alloc_string("f")?;
        let f = scope.alloc_user_function(
          CompiledFunctionRef {
            script: script.clone(),
            body: f_body,
            ast_fallback: None,
          },
          name,
          0,
        )?;
        let promise = vm.call_without_host(&mut scope, Value::Object(f), Value::Undefined, &[])?;
        scope.push_root(promise)?;
        scope.heap_mut().add_root(promise)
      })()
    };
    match promise_root_res {
      Ok(promise_root) => promise_root,
      Err(err) => {
        realm.teardown(&mut heap);
        return Err(err);
      }
    }
  };

  vm.perform_microtask_checkpoint(&mut heap)?;

  let promise = heap
    .get_root(promise_root)
    .ok_or(VmError::InvariantViolation("missing promise root"))?;
  let Value::Object(promise_obj) = promise else {
    panic!("expected promise object, got {promise:?}");
  };

  assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Fulfilled);
  let result = heap
    .promise_result(promise_obj)?
    .ok_or(VmError::InvariantViolation("missing promise result"))?;
  assert_eq!(result, Value::Number(6.0));

  heap.remove_root(promise_root);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn compiled_async_for_await_of_break_closes_iterator() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "compiled_async_for_await_of_break_closes.js",
    r#"
      async function f() {
        let closed = 0;
        const iterable = {
          [Symbol.asyncIterator]() {
            let i = 0;
            return {
              next() {
                if (i < 3) {
                  i = i + 1;
                  return Promise.resolve({ value: i, done: false });
                }
                return Promise.resolve({ done: true });
              },
              return() {
                closed = closed + 1;
                return Promise.resolve({ done: true });
              }
            };
          }
        };

        let iter = 0;
        for await (const x of iterable) {
          iter = iter + 1;
          if (iter === 2) break;
        }
        return closed;
      }
    "#,
  )?;
  let f_body = find_function_body(&script, "f");

  let mut vm = Vm::new(VmOptions::default());
  let mut realm = vm_js::Realm::new(&mut vm, &mut heap)?;

  let promise_root = {
    let promise_root_res: Result<_, VmError> = {
      let mut scope = heap.scope();
      (|| {
        let name = scope.alloc_string("f")?;
        let f = scope.alloc_user_function(
          CompiledFunctionRef {
            script: script.clone(),
            body: f_body,
            ast_fallback: None,
          },
          name,
          0,
        )?;
        let promise = vm.call_without_host(&mut scope, Value::Object(f), Value::Undefined, &[])?;
        scope.push_root(promise)?;
        scope.heap_mut().add_root(promise)
      })()
    };
    match promise_root_res {
      Ok(promise_root) => promise_root,
      Err(err) => {
        realm.teardown(&mut heap);
        return Err(err);
      }
    }
  };

  vm.perform_microtask_checkpoint(&mut heap)?;

  let promise = heap
    .get_root(promise_root)
    .ok_or(VmError::InvariantViolation("missing promise root"))?;
  let Value::Object(promise_obj) = promise else {
    panic!("expected promise object, got {promise:?}");
  };

  assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Fulfilled);
  let result = heap
    .promise_result(promise_obj)?
    .ok_or(VmError::InvariantViolation("missing promise result"))?;
  assert_eq!(result, Value::Number(1.0));

  heap.remove_root(promise_root);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn compiled_async_for_await_of_step_error_does_not_close_iterator() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "compiled_async_for_await_of_step_error_no_close.js",
    r#"
      async function f() {
        globalThis.closed = 0;
        const iterable = {
          [Symbol.asyncIterator]() {
            return {
              next() {
                return Promise.reject("err");
              },
              return() {
                globalThis.closed = globalThis.closed + 1;
                return Promise.resolve({ done: true });
              }
            };
          }
        };
        for await (const x of iterable) {
          // never reached
        }
      }
    "#,
  )?;
  let f_body = find_function_body(&script, "f");

  let mut vm = Vm::new(VmOptions::default());
  let mut realm = vm_js::Realm::new(&mut vm, &mut heap)?;

  let promise_root = {
    let promise_root_res: Result<_, VmError> = {
      let mut scope = heap.scope();
      (|| {
        let name = scope.alloc_string("f")?;
        let f = scope.alloc_user_function(
          CompiledFunctionRef {
            script: script.clone(),
            body: f_body,
            ast_fallback: None,
          },
          name,
          0,
        )?;
        let promise = vm.call_without_host(&mut scope, Value::Object(f), Value::Undefined, &[])?;
        scope.push_root(promise)?;
        scope.heap_mut().add_root(promise)
      })()
    };
    match promise_root_res {
      Ok(promise_root) => promise_root,
      Err(err) => {
        realm.teardown(&mut heap);
        return Err(err);
      }
    }
  };

  vm.perform_microtask_checkpoint(&mut heap)?;

  let promise = heap
    .get_root(promise_root)
    .ok_or(VmError::InvariantViolation("missing promise root"))?;
  let Value::Object(promise_obj) = promise else {
    panic!("expected promise object, got {promise:?}");
  };

  assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Rejected);
  let reason = heap
    .promise_result(promise_obj)?
    .ok_or(VmError::InvariantViolation("missing promise rejection reason"))?;
  let Value::String(reason_s) = reason else {
    panic!("expected promise rejection reason to be a string, got {reason:?}");
  };
  assert_eq!(heap.get_string(reason_s)?.to_utf8_lossy(), "err");

  // `return()` should not have been invoked for step errors.
  let closed_value = {
    let mut scope = heap.scope();
    let key_s = scope.alloc_string("closed")?;
    let key = PropertyKey::from_string(key_s);
    vm.get(&mut scope, realm.global_object(), key)?
  };
  assert_eq!(closed_value, Value::Number(0.0));

  heap.remove_root(promise_root);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn compiled_async_for_await_of_close_does_not_invoke_species() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "compiled_async_for_await_of_close_no_species.js",
    r#"
      async function f() {
        globalThis.speciesCalls = 0;
        class MyPromise extends Promise {
          static get [Symbol.species]() {
            globalThis.speciesCalls = globalThis.speciesCalls + 1;
            return Promise;
          }
        }

        const iterable = {
          [Symbol.asyncIterator]() {
            let i = 0;
            return {
              next() {
                if (i++ === 0) return Promise.resolve({ value: 1, done: false });
                return Promise.resolve({ done: true });
              },
              return() {
                return new MyPromise((resolve) => resolve({}));
              },
            };
          }
        };

        for await (const x of iterable) {
          break;
        }
        return globalThis.speciesCalls;
      }
    "#,
  )?;
  let f_body = find_function_body(&script, "f");

  let mut vm = Vm::new(VmOptions::default());
  let mut realm = vm_js::Realm::new(&mut vm, &mut heap)?;

  let promise_root = {
    let promise_root_res: Result<_, VmError> = {
      let mut scope = heap.scope();
      (|| {
        let name = scope.alloc_string("f")?;
        let f = scope.alloc_user_function(
          CompiledFunctionRef {
            script: script.clone(),
            body: f_body,
            ast_fallback: None,
          },
          name,
          0,
        )?;
        let promise = vm.call_without_host(&mut scope, Value::Object(f), Value::Undefined, &[])?;
        scope.push_root(promise)?;
        scope.heap_mut().add_root(promise)
      })()
    };
    match promise_root_res {
      Ok(promise_root) => promise_root,
      Err(err) => {
        realm.teardown(&mut heap);
        return Err(err);
      }
    }
  };

  vm.perform_microtask_checkpoint(&mut heap)?;

  let promise = heap
    .get_root(promise_root)
    .ok_or(VmError::InvariantViolation("missing promise root"))?;
  let Value::Object(promise_obj) = promise else {
    panic!("expected promise object, got {promise:?}");
  };

  assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Fulfilled);
  let result = heap
    .promise_result(promise_obj)?
    .ok_or(VmError::InvariantViolation("missing promise result"))?;
  assert_eq!(result, Value::Number(0.0));

  heap.remove_root(promise_root);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn compiled_async_for_await_of_close_constructor_getter_runs_once() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "compiled_async_for_await_of_close_constructor_getter_once.js",
    r#"
      async function f() {
        globalThis.calls = 0;
        const p = Promise.resolve({});
        Object.defineProperty(p, "constructor", {
          get() { globalThis.calls++; return Promise; }
        });

        const iterable = {
          [Symbol.asyncIterator]() {
            let i = 0;
            return {
              next() {
                if (i++ === 0) return Promise.resolve({ value: 1, done: false });
                return Promise.resolve({ done: true });
              },
              return() { return p; },
            };
          }
        };

        for await (const _x of iterable) {
          break;
        }
        return globalThis.calls;
      }
    "#,
  )?;
  let f_body = find_function_body(&script, "f");

  let mut vm = Vm::new(VmOptions::default());
  let mut realm = vm_js::Realm::new(&mut vm, &mut heap)?;

  let promise_root = {
    let promise_root_res: Result<_, VmError> = {
      let mut scope = heap.scope();
      (|| {
        let name = scope.alloc_string("f")?;
        let f = scope.alloc_user_function(
          CompiledFunctionRef {
            script: script.clone(),
            body: f_body,
            ast_fallback: None,
          },
          name,
          0,
        )?;
        let promise = vm.call_without_host(&mut scope, Value::Object(f), Value::Undefined, &[])?;
        scope.push_root(promise)?;
        scope.heap_mut().add_root(promise)
      })()
    };
    match promise_root_res {
      Ok(promise_root) => promise_root,
      Err(err) => {
        realm.teardown(&mut heap);
        return Err(err);
      }
    }
  };

  vm.perform_microtask_checkpoint(&mut heap)?;

  let promise = heap
    .get_root(promise_root)
    .ok_or(VmError::InvariantViolation("missing promise root"))?;
  let Value::Object(promise_obj) = promise else {
    panic!("expected promise object, got {promise:?}");
  };

  assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Fulfilled);
  let result = heap
    .promise_result(promise_obj)?
    .ok_or(VmError::InvariantViolation("missing promise result"))?;
  assert_eq!(result, Value::Number(1.0));

  heap.remove_root(promise_root);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn compiled_async_for_await_of_over_sync_iterator_awaits_values() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "compiled_async_for_await_of_over_sync_iterator_awaits_values.js",
    r#"
      async function f() {
        let sum = 0;
        for await (const x of [Promise.resolve(1), 2]) {
          sum += x;
        }
        return sum;
      }
    "#,
  )?;
  let f_body = find_function_body(&script, "f");

  let mut vm = Vm::new(VmOptions::default());
  let mut realm = vm_js::Realm::new(&mut vm, &mut heap)?;

  let promise_root = {
    let promise_root_res: Result<_, VmError> = {
      let mut scope = heap.scope();
      (|| {
        let name = scope.alloc_string("f")?;
        let f = scope.alloc_user_function(
          CompiledFunctionRef {
            script: script.clone(),
            body: f_body,
            ast_fallback: None,
          },
          name,
          0,
        )?;
        let promise = vm.call_without_host(&mut scope, Value::Object(f), Value::Undefined, &[])?;
        scope.push_root(promise)?;
        scope.heap_mut().add_root(promise)
      })()
    };
    match promise_root_res {
      Ok(promise_root) => promise_root,
      Err(err) => {
        realm.teardown(&mut heap);
        return Err(err);
      }
    }
  };

  vm.perform_microtask_checkpoint(&mut heap)?;

  let promise = heap
    .get_root(promise_root)
    .ok_or(VmError::InvariantViolation("missing promise root"))?;
  let Value::Object(promise_obj) = promise else {
    panic!("expected promise object, got {promise:?}");
  };

  assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Fulfilled);
  let result = heap
    .promise_result(promise_obj)?
    .ok_or(VmError::InvariantViolation("missing promise result"))?;
  assert_eq!(result, Value::Number(3.0));

  heap.remove_root(promise_root);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn compiled_async_for_await_of_break_closes_sync_iterator_via_async_from_sync_iterator() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "compiled_async_for_await_of_break_closes_sync_iterator.js",
    r#"
      async function f() {
        globalThis.closed = false;
        const iterable = {};
        iterable[Symbol.iterator] = function () {
          return {
            next() { return { value: 1, done: false }; },
            return() {
              // Side effect happens asynchronously to ensure `for await..of` awaits close.
              return Promise.resolve().then(function () {
                globalThis.closed = true;
                return { done: true };
              });
            },
          };
        };

        for await (const _x of iterable) {
          break;
        }
        return globalThis.closed;
      }
    "#,
  )?;
  let f_body = find_function_body(&script, "f");

  let mut vm = Vm::new(VmOptions::default());
  let mut realm = vm_js::Realm::new(&mut vm, &mut heap)?;

  let promise_root = {
    let mut scope = heap.scope();
    let name = scope.alloc_string("f")?;
    let f = scope.alloc_user_function(
      CompiledFunctionRef {
        script: script.clone(),
        body: f_body,
        ast_fallback: None,
      },
      name,
      0,
    )?;
    let promise = vm.call_without_host(&mut scope, Value::Object(f), Value::Undefined, &[])?;
    scope.push_root(promise)?;
    scope.heap_mut().add_root(promise)?
  };

  let result: Result<(), VmError> = (|| {
    vm.perform_microtask_checkpoint(&mut heap)?;

    let promise = heap
      .get_root(promise_root)
      .ok_or(VmError::InvariantViolation("missing promise root"))?;
    let Value::Object(promise_obj) = promise else {
      panic!("expected promise object, got {promise:?}");
    };

    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Fulfilled);
    let result = heap
      .promise_result(promise_obj)?
      .ok_or(VmError::InvariantViolation("missing promise result"))?;
    assert_eq!(result, Value::Bool(true));

    Ok(())
  })();

  heap.remove_root(promise_root);
  realm.teardown(&mut heap);
  result
}

#[test]
fn compiled_async_for_await_of_throw_closes_iterator_before_catch() -> Result<(), VmError> {
  // `for await..of` nested within a `try` block must await `AsyncIteratorClose` before the catch
  // clause runs.
  let mut heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "compiled_async_for_await_of_throw_closes_iterator_before_catch.js",
    r#"
      async function f() {
        let closed = false;
        const iterable = {};
        iterable[Symbol.asyncIterator] = function () {
          return {
            next() {
              return Promise.resolve({ value: 1, done: false });
            },
            return() {
              // Close asynchronously so catch order is observable.
              return Promise.resolve().then(function () {
                closed = true;
                return { done: true };
              });
            },
          };
        };

        let out = false;
        try {
          for await (const _x of iterable) {
            throw "boom";
          }
        } catch (e) {
          out = closed;
        }
        return out;
      }
    "#,
  )?;
  let f_body = find_function_body(&script, "f");

  let mut vm = Vm::new(VmOptions::default());
  let mut realm = vm_js::Realm::new(&mut vm, &mut heap)?;

  let promise_root = {
    let mut scope = heap.scope();
    let name = scope.alloc_string("f")?;
    let f = scope.alloc_user_function(
      CompiledFunctionRef {
        script: script.clone(),
        body: f_body,
        ast_fallback: None,
      },
      name,
      0,
    )?;
    let promise = vm.call_without_host(&mut scope, Value::Object(f), Value::Undefined, &[])?;
    scope.push_root(promise)?;
    scope.heap_mut().add_root(promise)?
  };

  vm.perform_microtask_checkpoint(&mut heap)?;

  let promise = heap
    .get_root(promise_root)
    .ok_or(VmError::InvariantViolation("missing promise root"))?;
  let Value::Object(promise_obj) = promise else {
    panic!("expected promise object, got {promise:?}");
  };

  assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Fulfilled);
  let result = heap
    .promise_result(promise_obj)?
    .ok_or(VmError::InvariantViolation("missing promise result"))?;
  assert_eq!(result, Value::Bool(true));

  heap.remove_root(promise_root);
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn compiled_async_for_await_of_return_closes_iterator_before_finally() -> Result<(), VmError> {
  // `for await..of` nested within a `try` block is currently not supported by the compiled async
  // executor (HIR). Ensure we exercise the intended call-time AST fallback by executing the
  // compiled script normally and calling the resulting function object.
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  let script = CompiledScript::compile_script(
    &mut rt.heap,
    "compiled_async_for_await_of_return_closes_iterator_before_finally.js",
    r#"
      async function f() {
        let closed = false;
        const iterable = {};
        iterable[Symbol.asyncIterator] = function () {
          return {
            next() {
              return Promise.resolve({ value: 1, done: false });
            },
            return() {
              // Close asynchronously so finally ordering is observable.
              return Promise.resolve().then(function () {
                closed = true;
                return { done: true };
              });
            },
          };
        };

        try {
          for await (const _x of iterable) {
            return "ignored";
          }
        } finally {
          // If AsyncIteratorClose is correctly awaited, this must be true.
          return closed;
        }
      }
      f;
    "#,
  )?;

  let result = rt.exec_compiled_script(script)?;
  let Value::Object(func_obj) = result else {
    panic!("expected compiled script to evaluate to a function object, got {result:?}");
  };

  let promise = {
    let mut scope = rt.heap.scope();
    rt.vm
      .call_without_host(&mut scope, Value::Object(func_obj), Value::Undefined, &[])?
  };
  let promise_root = rt.heap.add_root(promise)?;

  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  let Some(Value::Object(promise_obj)) = rt.heap.get_root(promise_root) else {
    panic!("expected async function call to return a Promise object");
  };
  assert_eq!(rt.heap.promise_state(promise_obj)?, PromiseState::Fulfilled);
  assert_eq!(
    rt.heap
      .promise_result(promise_obj)?
      .ok_or(VmError::InvariantViolation("missing promise result"))?,
    Value::Bool(true)
  );
  rt.heap.remove_root(promise_root);
  Ok(())
}

#[test]
fn compiled_async_for_await_of_throw_close_rejection_is_suppressed() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "compiled_async_for_await_of_throw_close_rejection_is_suppressed.js",
    r#"
      async function f() {
        const iterable = {};
        iterable[Symbol.asyncIterator] = function () {
          return {
            next() {
              return Promise.resolve({ value: 1, done: false });
            },
            return() {
              return Promise.reject("closeerr");
            },
          };
        };

        for await (const _x of iterable) {
          throw "bodyerr";
        }
      }
    "#,
  )?;
  let f_body = find_function_body(&script, "f");

  let mut vm = Vm::new(VmOptions::default());
  let mut realm = vm_js::Realm::new(&mut vm, &mut heap)?;

  let promise_root = {
    let promise_root_res: Result<_, VmError> = {
      let mut scope = heap.scope();
      (|| {
        let name = scope.alloc_string("f")?;
        let f = scope.alloc_user_function(
          CompiledFunctionRef {
            script: script.clone(),
            body: f_body,
            ast_fallback: None,
          },
          name,
          0,
        )?;
        let promise = vm.call_without_host(&mut scope, Value::Object(f), Value::Undefined, &[])?;
        scope.push_root(promise)?;
        scope.heap_mut().add_root(promise)
      })()
    };
    match promise_root_res {
      Ok(promise_root) => promise_root,
      Err(err) => {
        realm.teardown(&mut heap);
        return Err(err);
      }
    }
  };

  let result: Result<(), VmError> = (|| {
    vm.perform_microtask_checkpoint(&mut heap)?;

    let promise = heap
      .get_root(promise_root)
      .ok_or(VmError::InvariantViolation("missing promise root"))?;
    let Value::Object(promise_obj) = promise else {
      panic!("expected promise object, got {promise:?}");
    };

    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Rejected);
    let reason = heap
      .promise_result(promise_obj)?
      .ok_or(VmError::InvariantViolation("missing promise rejection reason"))?;
    let Value::String(reason_s) = reason else {
      panic!("expected promise rejection reason to be a string, got {reason:?}");
    };
    assert_eq!(heap.get_string(reason_s)?.to_utf8_lossy(), "bodyerr");

    Ok(())
  })();

  heap.remove_root(promise_root);
  realm.teardown(&mut heap);
  result
}

#[test]
fn compiled_async_for_await_of_break_close_rejection_overrides_completion() -> Result<(), VmError> {
  let mut heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  let script = CompiledScript::compile_script(
    &mut heap,
    "compiled_async_for_await_of_break_close_rejection_overrides_completion.js",
    r#"
      async function f() {
        const iterable = {};
        iterable[Symbol.asyncIterator] = function () {
          return {
            next() {
              return Promise.resolve({ value: 1, done: false });
            },
            return() {
              return Promise.reject("closeerr");
            },
          };
        };

        for await (const _x of iterable) {
          break;
        }
      }
    "#,
  )?;
  let f_body = find_function_body(&script, "f");

  let mut vm = Vm::new(VmOptions::default());
  let mut realm = vm_js::Realm::new(&mut vm, &mut heap)?;

  let promise_root = {
    let promise_root_res: Result<_, VmError> = {
      let mut scope = heap.scope();
      (|| {
        let name = scope.alloc_string("f")?;
        let f = scope.alloc_user_function(
          CompiledFunctionRef {
            script: script.clone(),
            body: f_body,
            ast_fallback: None,
          },
          name,
          0,
        )?;
        let promise = vm.call_without_host(&mut scope, Value::Object(f), Value::Undefined, &[])?;
        scope.push_root(promise)?;
        scope.heap_mut().add_root(promise)
      })()
    };
    match promise_root_res {
      Ok(promise_root) => promise_root,
      Err(err) => {
        realm.teardown(&mut heap);
        return Err(err);
      }
    }
  };

  let result: Result<(), VmError> = (|| {
    vm.perform_microtask_checkpoint(&mut heap)?;

    let promise = heap
      .get_root(promise_root)
      .ok_or(VmError::InvariantViolation("missing promise root"))?;
    let Value::Object(promise_obj) = promise else {
      panic!("expected promise object, got {promise:?}");
    };

    assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Rejected);
    let reason = heap
      .promise_result(promise_obj)?
      .ok_or(VmError::InvariantViolation("missing promise rejection reason"))?;
    let Value::String(reason_s) = reason else {
      panic!("expected promise rejection reason to be a string, got {reason:?}");
    };
    assert_eq!(heap.get_string(reason_s)?.to_utf8_lossy(), "closeerr");

    Ok(())
  })();

  heap.remove_root(promise_root);
  realm.teardown(&mut heap);
  result
}
