use crate::{
  exec::RuntimeEnv,
  CompiledScript, ExecutionContext, Heap, HeapLimits, MicrotaskQueue, PromiseState, PropertyKey, Realm, RootId,
  ScriptOrModule, StackFrame, Value, Vm, VmError, VmHost, VmHostHooks, VmJobContext, VmOptions,
};
use std::sync::Arc;

fn value_to_string(heap: &Heap, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  heap.get_string(s).unwrap().to_utf8_lossy()
}

fn get_global(vm: &mut Vm, heap: &mut Heap, global: crate::GcObject, name: &str) -> Result<Value, VmError> {
  let mut scope = heap.scope();
  let key_s = scope.alloc_string(name)?;
  let key = PropertyKey::from_string(key_s);
  vm.get(&mut scope, global, key)
}

fn perform_microtask_checkpoint_with_hooks(
  vm: &mut Vm,
  heap: &mut Heap,
  host: &mut dyn VmHost,
  hooks: &mut MicrotaskQueue,
) -> Result<(), VmError> {
  struct Ctx<'a> {
    vm: &'a mut Vm,
    host: &'a mut dyn VmHost,
    heap: &'a mut Heap,
  }

  impl VmJobContext for Ctx<'_> {
    fn call(
      &mut self,
      hooks: &mut dyn VmHostHooks,
      callee: Value,
      this: Value,
      args: &[Value],
    ) -> Result<Value, VmError> {
      let mut scope = self.heap.scope();
      self
        .vm
        .call_with_host_and_hooks(&mut *self.host, &mut scope, hooks, callee, this, args)
    }

    fn construct(
      &mut self,
      hooks: &mut dyn VmHostHooks,
      callee: Value,
      args: &[Value],
      new_target: Value,
    ) -> Result<Value, VmError> {
      let mut scope = self.heap.scope();
      self.vm.construct_with_host_and_hooks(
        &mut *self.host,
        &mut scope,
        hooks,
        callee,
        args,
        new_target,
      )
    }

    fn add_root(&mut self, value: Value) -> Result<RootId, VmError> {
      self.heap.add_root(value)
    }

    fn remove_root(&mut self, id: RootId) {
      self.heap.remove_root(id);
    }

    fn coerce_error_to_throw_with_stack(&mut self, err: VmError) -> VmError {
      let mut scope = self.heap.scope();
      crate::vm::coerce_error_to_throw_with_stack(&*self.vm, &mut scope, err)
    }
  }

  let mut ctx = Ctx { vm, host, heap };
  let errors = hooks.perform_microtask_checkpoint(&mut ctx);
  if errors.is_empty() {
    Ok(())
  } else {
    // Report the first job failure; remaining errors (if any) are ignored.
    Err(errors.into_iter().next().unwrap())
  }
}

fn run_compiled_hir_script(
  vm: &mut Vm,
  heap: &mut Heap,
  host: &mut dyn VmHost,
  hooks: &mut MicrotaskQueue,
  env: &mut RuntimeEnv,
  script: Arc<CompiledScript>,
) -> Result<Value, VmError> {
  let source = script.source.clone();
  let (line, col) = source.line_col(0);
  let frame = StackFrame {
    function: None,
    source: source.name.clone(),
    line,
    col,
  };

  // Charge at least one tick at script entry.
  vm.tick()?;

  let mut vm_frame = vm.enter_frame(frame)?;
  let mut scope = heap.scope();
  crate::hir_exec::run_compiled_script(&mut *vm_frame, &mut scope, host, hooks, env, script)
}

#[test]
fn compiled_hir_top_level_for_await_of_break_closes_iterator_and_updates_completion_value() -> Result<(), VmError> {
  // Top-level await execution allocates Promise/job machinery; use a slightly larger heap than
  // the minimal 1MiB used by some unit tests to avoid spurious OOMs as builtin surface area grows.
  let mut heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  let mut vm = Vm::new(VmOptions::default());
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let script = CompiledScript::compile_script(
    &mut heap,
    "compiled_hir_top_level_for_await_of_break.js",
    r#"
      var out = "";
      var closed = 0;

      var iter = {
        i: 0,
        next: function () {
          if (this.i++ === 0) return Promise.resolve({ value: "ok", done: false });
          return Promise.resolve({ value: undefined, done: true });
        },
        return: function () {
          closed = closed + 1;
          return Promise.resolve({ done: true });
        },
      };
      var iterable = {};
      iterable[Symbol.asyncIterator] = function () { return iter; };

      for await (var x of iterable) {
        out = x;
        break;
      }
      out
    "#,
  )?;

  let mut env = RuntimeEnv::new_with_lexical_env(&mut heap, realm.global_object(), realm.global_lexical_env())?;
  let mut host = ();
  let mut hooks = MicrotaskQueue::new();

  // Establish a script execution context so jobs created by the compiled executor capture a realm
  // (and so resume callbacks can re-enter with consistent `GetActiveScriptOrModule` semantics).
  let exec_ctx = ExecutionContext {
    realm: realm.id(),
    script_or_module: Some(ScriptOrModule::Script(vm.fresh_script_id()?)),
  };
  vm.push_execution_context(exec_ctx)?;
  let prev_state = vm.load_realm_state(&mut heap, exec_ctx.realm)?;

  let completion = run_compiled_hir_script(&mut vm, &mut heap, &mut host, &mut hooks, &mut env, script)?;
  let completion_root = {
    let mut scope = heap.scope();
    scope.push_root(completion)?;
    scope.heap_mut().add_root(completion)?
  };

  let Value::Object(promise_obj) = completion else {
    panic!("expected Promise object from async script, got {completion:?}");
  };
  assert!(heap.is_promise_object(promise_obj));
  assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Pending);

  // Loop body should not have executed until we run microtasks.
  let out = get_global(&mut vm, &mut heap, realm.global_object(), "out")?;
  assert_eq!(value_to_string(&heap, out), "");
  let closed = get_global(&mut vm, &mut heap, realm.global_object(), "closed")?;
  assert_eq!(closed, Value::Number(0.0));

  perform_microtask_checkpoint_with_hooks(&mut vm, &mut heap, &mut host, &mut hooks)?;

  assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Fulfilled);
  let result = heap
    .promise_result(promise_obj)?
    .expect("fulfilled promise should have a result");
  assert_eq!(value_to_string(&heap, result), "ok");

  let out = get_global(&mut vm, &mut heap, realm.global_object(), "out")?;
  assert_eq!(value_to_string(&heap, out), "ok");
  let closed = get_global(&mut vm, &mut heap, realm.global_object(), "closed")?;
  assert_eq!(closed, Value::Number(1.0));

  heap.remove_root(completion_root);
  env.teardown(&mut heap);
  let popped = vm.pop_execution_context();
  debug_assert_eq!(popped, Some(exec_ctx));
  vm.restore_realm_state(&mut heap, prev_state)?;
  realm.teardown(&mut heap);
  Ok(())
}

#[test]
fn compiled_hir_top_level_for_await_of_close_rejection_does_not_override_pending_throw() -> Result<(), VmError> {
  // This specifically covers `hir_async_resume_call` rejection handling: when awaiting
  // `AsyncIteratorClose` in a `for await..of`, a rejected close promise must be observed by the loop
  // state so it can apply the spec's error-precedence rules.
  let mut heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  let mut vm = Vm::new(VmOptions::default());
  let mut realm = Realm::new(&mut vm, &mut heap)?;

  let script = CompiledScript::compile_script(
    &mut heap,
    "compiled_hir_top_level_for_await_of_close_reject_precedence.js",
    r#"
      var closed = 0;
      var iter = {
        i: 0,
        next: function () {
          if (this.i++ === 0) return Promise.resolve({ value: "ok", done: false });
          return Promise.resolve({ value: undefined, done: true });
        },
        return: function () {
          closed = closed + 1;
          return Promise.reject("close");
        },
      };
      var iterable = {};
      iterable[Symbol.asyncIterator] = function () { return iter; };

      for await (var x of iterable) {
        throw "boom";
      }
    "#,
  )?;

  let mut env = RuntimeEnv::new_with_lexical_env(&mut heap, realm.global_object(), realm.global_lexical_env())?;
  let mut host = ();
  let mut hooks = MicrotaskQueue::new();

  let exec_ctx = ExecutionContext {
    realm: realm.id(),
    script_or_module: Some(ScriptOrModule::Script(vm.fresh_script_id()?)),
  };
  vm.push_execution_context(exec_ctx)?;
  let prev_state = vm.load_realm_state(&mut heap, exec_ctx.realm)?;

  let completion = run_compiled_hir_script(&mut vm, &mut heap, &mut host, &mut hooks, &mut env, script)?;
  let completion_root = {
    let mut scope = heap.scope();
    scope.push_root(completion)?;
    scope.heap_mut().add_root(completion)?
  };

  let Value::Object(promise_obj) = completion else {
    panic!("expected Promise object from async script, got {completion:?}");
  };
  assert!(heap.is_promise_object(promise_obj));
  assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Pending);

  // The loop body and iterator closing should not have run yet.
  let closed = get_global(&mut vm, &mut heap, realm.global_object(), "closed")?;
  assert_eq!(closed, Value::Number(0.0));

  perform_microtask_checkpoint_with_hooks(&mut vm, &mut heap, &mut host, &mut hooks)?;

  assert_eq!(heap.promise_state(promise_obj)?, PromiseState::Rejected);
  let reason = heap
    .promise_result(promise_obj)?
    .expect("rejected promise should have a rejection reason");
  assert_eq!(value_to_string(&heap, reason), "boom");

  // `return()` should have been invoked despite its rejected Promise result.
  let closed = get_global(&mut vm, &mut heap, realm.global_object(), "closed")?;
  assert_eq!(closed, Value::Number(1.0));

  heap.remove_root(completion_root);
  env.teardown(&mut heap);
  let popped = vm.pop_execution_context();
  debug_assert_eq!(popped, Some(exec_ctx));
  vm.restore_realm_state(&mut heap, prev_state)?;
  realm.teardown(&mut heap);
  Ok(())
}
