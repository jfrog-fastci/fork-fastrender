use inkwell::context::Context;
use inkwell::targets::{CodeModel, RelocMode, Target, TargetMachine};
use inkwell::OptimizationLevel;
use inkwell::values::AsValueRef as _;
use llvm_sys::core::LLVMGetStringAttributeAtIndex;
use llvm_sys::LLVMAttributeFunctionIndex;
use native_js::llvm::{gc, passes};
use native_js::runtime_abi::{RuntimeAbi, RuntimeFn};

fn function_block(ir: &str, func_name: &str) -> String {
  let mut out = Vec::new();
  let mut in_func = false;

  for line in ir.lines() {
    if !in_func && line.contains("define") && line.contains(func_name) {
      in_func = true;
    }

    if in_func {
      out.push(line);
      if line.trim() == "}" {
        break;
      }
    }
  }

  assert!(in_func, "function {func_name} not found in IR:\n{ir}");
  out.join("\n")
}

fn has_gc_leaf_attr(func: inkwell::values::FunctionValue<'_>) -> bool {
  // Mirror the convention used by LLVM's `rewrite-statepoints-for-gc` pass.
  const KEY: &[u8] = b"gc-leaf-function\0";
  unsafe {
    !LLVMGetStringAttributeAtIndex(
      func.as_value_ref(),
      LLVMAttributeFunctionIndex,
      KEY.as_ptr().cast(),
      (KEY.len() - 1) as u32,
    )
    .is_null()
  }
}

#[test]
fn runtime_async_and_promise_calls_have_correct_statepoint_and_handle_abi() {
  native_js::llvm::init_native_target().expect("failed to init native target");

  let context = Context::create();
  let module = context.create_module("runtime_async_abi");
  let builder = context.create_builder();

  let rt = RuntimeAbi::new(&context, &module);
  rt.declare_all();

  // define void @test() gc "coreclr"
  let test_ty = context.void_type().fn_type(&[], false);
  let test_fn = module.add_function("test", test_ty, None);
  gc::set_default_gc_strategy(&test_fn).expect("GC strategy contains NUL byte");

  let entry = context.append_basic_block(test_fn, "entry");
  builder.position_at_end(entry);

  // NoGC call: must remain a normal call and the callee must be marked as a GC leaf function.
  rt
    .emit_runtime_call(
      &builder,
      RuntimeFn::AsyncSetStrictAwaitYields,
      &[context.bool_type().const_int(1, false).into()],
      "strict",
    )
    .expect("emit rt_async_set_strict_await_yields");

  let strict = module
    .get_function("rt_async_set_strict_await_yields")
    .expect("rt_async_set_strict_await_yields declared");
  assert!(
    has_gc_leaf_attr(strict),
    "expected rt_async_set_strict_await_yields to be marked gc-leaf-function"
  );

  // MayGC call with no GC pointer args: must become a statepoint.
  let coro_id = context.i64_type().const_zero();
  let promise = rt
    .emit_runtime_call(&builder, RuntimeFn::AsyncSpawn, &[coro_id.into()], "spawn")
    .expect("emit rt_async_spawn")
    .try_as_basic_value()
    .left()
    .expect("rt_async_spawn returns a value")
    .into_pointer_value();

  // MayGC call with a GC pointer arg: must be allowed only when ArgRootingPolicy::RuntimeRootsPointers
  // is set in the RuntimeFn registry.
  rt
    .emit_runtime_call(
      &builder,
      RuntimeFn::PromiseFulfill,
      &[promise.into()],
      "fulfill",
    )
    .expect("emit rt_promise_fulfill");

  // MayGC call with handle arg: must pass `ptr %slot` (pointer-to-slot, addrspace(0)).
  let gc_ptr_ty = gc::gc_ptr_type(&context);
  let slot = builder
    .build_alloca(gc_ptr_ty, "slot")
    .expect("alloca slot for handle ABI");
  builder.build_store(slot, promise).expect("store promise to slot");

  let _handle = rt
    .emit_runtime_call(&builder, RuntimeFn::HandleAllocH, &[slot.into()], "handle")
    .expect("emit rt_handle_alloc_h");

  builder.build_return(None).expect("ret void");

  if let Err(err) = module.verify() {
    panic!(
      "module verification failed before rewrite-statepoints-for-gc: {err}\n\nIR:\n{}",
      module.print_to_string()
    );
  }

  let triple = TargetMachine::get_default_triple();
  let target = Target::from_triple(&triple).expect("host target");
  let tm = target
    .create_target_machine(
      &triple,
      "generic",
      "",
      OptimizationLevel::None,
      RelocMode::Default,
      CodeModel::Default,
    )
    .expect("create target machine");
  module.set_triple(&triple);
  module.set_data_layout(&tm.get_target_data().get_data_layout());

  passes::rewrite_statepoints_for_gc(&module, &tm).unwrap_or_else(|err| {
    panic!(
      "rewrite-statepoints-for-gc failed: {err}\n\nAfter:\n{}",
      module.print_to_string()
    )
  });

  if let Err(err) = module.verify() {
    panic!(
      "module verification failed after rewrite-statepoints-for-gc: {err}\n\nIR:\n{}",
      module.print_to_string()
    );
  }

  let rewritten = module.print_to_string().to_string();
  let func = function_block(&rewritten, "@test");

  // MayGC calls must be statepointed.
  for name in ["rt_async_spawn", "rt_promise_fulfill", "rt_handle_alloc_h"] {
    let line = func
      .lines()
      .find(|l| l.contains("@llvm.experimental.gc.statepoint") && l.contains(name))
      .unwrap_or_else(|| panic!("missing statepointed call to {name} in:\n{func}"));
    assert!(
      line.contains(name),
      "statepoint line should reference {name}, got:\n{line}\n\n{func}"
    );
  }

  // NoGC call must remain a normal call (not a statepoint).
  assert!(
    func.contains("notail call void @rt_async_set_strict_await_yields"),
    "expected rt_async_set_strict_await_yields to remain a normal call:\n{func}"
  );
  assert!(
    !func
      .lines()
      .any(|l| l.contains("@llvm.experimental.gc.statepoint") && l.contains("rt_async_set_strict_await_yields")),
    "rt_async_set_strict_await_yields must not be statepointed:\n{func}"
  );

  // Handle ABI call must pass the slot address (`ptr %slot`), not a GC pointer.
  let handle_statepoint = func
    .lines()
    .find(|l| l.contains("@llvm.experimental.gc.statepoint") && l.contains("rt_handle_alloc_h"))
    .expect("statepointed call to rt_handle_alloc_h");
  assert!(
    handle_statepoint.contains("ptr %slot"),
    "expected handle call to pass `ptr %slot`, got:\n{handle_statepoint}\n\n{func}"
  );
  assert!(
    !handle_statepoint.contains("ptr addrspace(1) %slot"),
    "handle slot must be addrspace(0) `ptr`, got:\n{handle_statepoint}\n\n{func}"
  );
}

