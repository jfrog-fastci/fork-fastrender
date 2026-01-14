use vm_js::{
  CompiledFunctionRef, CompiledScript, Heap, HeapLimits, JsRuntime, PropertyDescriptor, PropertyKey,
  PropertyKind, Value, Vm, VmError, VmOptions,
};

#[test]
fn compiled_user_function_call_falls_back_for_async_and_generator_bodies() -> Result<(), VmError> {
  let vm = Vm::new(VmOptions::default());
  // Async/Promise machinery can allocate a bit; use a moderately sized heap to avoid flakiness.
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap)?;

  // Compile a script containing both an async function and a generator function. We will extract
  // their HIR body ids and invoke them via `CallHandler::User` to ensure the VM transparently
  // falls back to the AST evaluator for unsupported HIR constructs (`await`/`yield`).
  let script = CompiledScript::compile_script(
    rt.heap_mut(),
    "compiled_hir_fallback_async_and_generator.js",
    r#"
      async function af() { return 1; }
      function* g() { yield 2; }
    "#,
  )?;

  let (af_body, g_body) = {
    let hir = script.hir.as_ref();
    let mut af: Option<hir_js::BodyId> = None;
    let mut g: Option<hir_js::BodyId> = None;
    for (body_id, idx) in hir.body_index.iter() {
      let body = hir
        .bodies
        .get(*idx)
        .ok_or(VmError::InvariantViolation("hir body index out of bounds"))?;
      if body.kind != hir_js::BodyKind::Function {
        continue;
      }
      let Some(meta) = body.function.as_ref() else {
        continue;
      };
      let def = hir
        .def(body.owner)
        .ok_or(VmError::InvariantViolation("hir def id missing from compiled script"))?;
      let name = hir.names.resolve(def.name).unwrap_or("<missing>");
      if meta.async_ && !meta.generator && name == "af" {
        af = Some(*body_id);
      }
      if meta.generator && !meta.async_ && name == "g" {
        g = Some(*body_id);
      }
    }
    (
      af.ok_or(VmError::InvariantViolation(
        "async function body not found in compiled script",
      ))?,
      g.ok_or(VmError::InvariantViolation(
        "generator function body not found in compiled script",
      ))?,
    )
  };

  // Define both compiled functions on the global object.
  {
    let global = rt.realm().global_object();
    let intr = rt.realm().intrinsics();
    let gen_proto = intr.generator_prototype();

    let mut scope = rt.heap_mut().scope();
    scope.push_root(Value::Object(global))?;

    // Allocate a compiled user function for the async body.
    let af_name = scope.alloc_string("af")?;
    scope.push_root(Value::String(af_name))?;
    let af_obj = scope.alloc_user_function(
      CompiledFunctionRef {
        script: script.clone(),
        body: af_body,
        ast_fallback: None,
      },
      af_name,
      0,
    )?;
    scope.push_root(Value::Object(af_obj))?;

    // Allocate a compiled user function for the generator body.
    let g_name = scope.alloc_string("g")?;
    scope.push_root(Value::String(g_name))?;
    let g_obj = scope.alloc_user_function(
      CompiledFunctionRef {
        script: script.clone(),
        body: g_body,
        ast_fallback: None,
      },
      g_name,
      0,
    )?;
    scope.push_root(Value::Object(g_obj))?;

    // `Scope::alloc_user_function` always creates an ordinary function `.prototype` object. However,
    // generator *calls* use `F.prototype` as the prototype for the returned generator object
    // (`OrdinaryCreateFromConstructor`). If it does not inherit from `%GeneratorPrototype%`, the
    // returned iterator would be missing `.next()`.
    //
    // Patch the test function's `.prototype` to `%GeneratorPrototype%` so `g().next()` works even
    // though we intentionally allocate `g` as a compiled user function.
    let proto_key_s = scope.alloc_string("prototype")?;
    scope.push_root(Value::String(proto_key_s))?;
    let proto_key = PropertyKey::from_string(proto_key_s);
    scope.define_property(
      g_obj,
      proto_key,
      PropertyDescriptor {
        enumerable: false,
        configurable: false,
        kind: PropertyKind::Data {
          value: Value::Object(gen_proto),
          writable: true,
        },
      },
    )?;

    for (name_s, func_obj) in [(af_name, af_obj), (g_name, g_obj)] {
      let key = PropertyKey::from_string(name_s);
      scope.define_property(
        global,
        key,
        PropertyDescriptor {
          enumerable: true,
          configurable: true,
          kind: PropertyKind::Data {
            value: Value::Object(func_obj),
            writable: true,
          },
        },
      )?;
    }
  }

  rt.exec_script(
    r#"
      var asyncResult = 0;
      var genResult = 0;
      af().then(v => { asyncResult = v; });
      genResult = g().next().value;
    "#,
  )?;
  rt.vm.perform_microtask_checkpoint(&mut rt.heap)?;

  assert_eq!(rt.exec_script("asyncResult")?, Value::Number(1.0));
  assert_eq!(rt.exec_script("genResult")?, Value::Number(2.0));
  Ok(())
}
