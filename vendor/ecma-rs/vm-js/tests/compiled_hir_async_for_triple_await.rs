use std::sync::Arc;
use vm_js::{
  CompiledFunctionRef, CompiledScript, Heap, HeapLimits, PromiseState, Value, Vm, VmError, VmOptions,
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

fn run_async_fn_case<T>(script_src: &str, map: impl FnOnce(&Heap, Value) -> T) -> Result<T, VmError> {
  // Promise/async-await allocates builtin job machinery; use a larger heap to avoid spurious OOMs.
  let mut heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  let script = CompiledScript::compile_script(&mut heap, "<inline>", script_src)?;
  let body_id = find_function_body(&script, "f");

  let mut vm = Vm::new(VmOptions::default());
  let mut realm = vm_js::Realm::new(&mut vm, &mut heap)?;

  let promise_root = {
    let mut scope = heap.scope();
    let name = scope.alloc_string("f")?;
    let f = scope.alloc_user_function(
      CompiledFunctionRef {
        script: script.clone(),
        body: body_id,
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

  let promise_state = heap.promise_state(promise_obj)?;
  let promise_result = heap.promise_result(promise_obj)?;
  let resolved = match promise_state {
    PromiseState::Fulfilled => promise_result
      .ok_or(VmError::InvariantViolation("missing promise result"))?,
    PromiseState::Rejected => {
      panic!(
        "expected async function Promise to fulfill, got Rejected (reason={promise_result:?})"
      );
    }
    PromiseState::Pending => {
      panic!("expected async function Promise to settle after microtask checkpoint");
    }
  };

  let mapped = map(&heap, resolved);
  heap.remove_root(promise_root);
  realm.teardown(&mut heap);
  Ok(mapped)
}

#[test]
fn compiled_async_await_in_for_triple_init() -> Result<(), VmError> {
  let out = run_async_fn_case(
    r#"
      async function f(){
        let i = await Promise.resolve(0);
        let out='';
        for (i = await Promise.resolve(0); i < 2; i++) { out += i; }
        return out;
      }
    "#,
    |heap, value| {
      let Value::String(s) = value else {
        panic!("expected string, got {value:?}");
      };
      heap.get_string(s).unwrap().to_utf8_lossy()
    },
  )?;
  assert_eq!(out, "01");
  Ok(())
}

#[test]
fn compiled_async_await_in_for_triple_init_var_decl() -> Result<(), VmError> {
  let out = run_async_fn_case(
    r#"
      async function f(){
        let out='';
        for (let i = await Promise.resolve(0); i < 2; i++) { out += i; }
        return out;
      }
    "#,
    |heap, value| {
      let Value::String(s) = value else {
        panic!("expected string, got {value:?}");
      };
      heap.get_string(s).unwrap().to_utf8_lossy()
    },
  )?;
  assert_eq!(out, "01");
  Ok(())
}

#[test]
fn compiled_async_await_in_for_triple_test() -> Result<(), VmError> {
  let value = run_async_fn_case(
    r#"
      async function f(){
        let i=0;
        while(false){}
        for (; await Promise.resolve(i<3); i++) {}
        return i;
      }
    "#,
    |_heap, value| value,
  )?;
  assert_eq!(value, Value::Number(3.0));
  Ok(())
}

#[test]
fn compiled_async_await_in_for_triple_update() -> Result<(), VmError> {
  let value = run_async_fn_case(
    r#"
      async function f(){
        let i=0;
        for (; i<3; i = await Promise.resolve(i+1)) {}
        return i;
      }
    "#,
    |_heap, value| value,
  )?;
  assert_eq!(value, Value::Number(3.0));
  Ok(())
}

#[test]
fn compiled_async_for_triple_await_break_continue() -> Result<(), VmError> {
  let out = run_async_fn_case(
    r#"
      async function f(){
        let out='';
        for (let i=0; await Promise.resolve(i<3); i = await Promise.resolve(i+1)) {
          if (i===1) continue;
          if (i===2) break;
          out += i;
        }
        return out;
      }
    "#,
    |heap, value| {
      let Value::String(s) = value else {
        panic!("expected string, got {value:?}");
      };
      heap.get_string(s).unwrap().to_utf8_lossy()
    },
  )?;
  assert_eq!(out, "0");
  Ok(())
}
