use inkwell::context::Context;
use inkwell::memory_buffer::MemoryBuffer;
use inkwell::targets::{CodeModel, InitializationConfig, RelocMode, Target, TargetMachine};
use inkwell::OptimizationLevel;
use native_js::llvm::passes;

#[test]
fn llvm18_statepoint_rewrite_indirect_call_has_elementtype() {
  // In opaque-pointer mode (LLVM >= 15, and default in LLVM 18), the callee operand of
  // `llvm.experimental.gc.statepoint` must carry an `elementtype(<fn-ty>)`.
  //
  // This is especially important for *indirect calls* through a `ptr`-typed function pointer:
  // the call site's signature must be propagated to the statepoint's callee operand.
  let input_ir = r#"
; ModuleID = 'statepoints_indirect_call'
source_filename = "statepoints_indirect_call.ll"

declare void @callee(i64)

define void @test(ptr addrspace(1) %obj) gc "statepoint-example" {
entry:
  %fp_slot = alloca ptr, align 8
  store ptr @callee, ptr %fp_slot, align 8
  %fp = load ptr, ptr %fp_slot, align 8
  call void %fp(i64 123)
  %isnull = icmp eq ptr addrspace(1) %obj, null
  ret void
}
"#;

  Target::initialize_native(&InitializationConfig::default()).expect("failed to initialize native LLVM target");

  let context = Context::create();
  let buffer = MemoryBuffer::create_from_memory_range_copy(input_ir.as_bytes(), "statepoints_indirect_call.ll");
  let module = context
    .create_module_from_ir(buffer)
    .unwrap_or_else(|err| panic!("failed to parse LLVM IR: {err}\n\nIR:\n{input_ir}"));

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
      "rewrite-statepoints-for-gc failed: {err}\n\nBefore:\n{input_ir}\n\nAfter:\n{}",
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

  // Statepoint inserted.
  assert!(
    rewritten.contains("llvm.experimental.gc.statepoint.p0"),
    "expected gc.statepoint intrinsic in rewritten IR, got:\n{rewritten}"
  );

  // Indirect call's callee operand must carry elementtype(void (i64)).
  assert!(
    rewritten.contains("ptr elementtype(void (i64)) %fp"),
    "expected statepoint callee operand to be `ptr elementtype(void (i64)) %fp`, got:\n{rewritten}"
  );

  // %obj is live across the call => it must be in the gc-live bundle.
  assert!(
    rewritten.contains("\"gc-live\"(ptr addrspace(1) %obj)"),
    "expected `\"gc-live\"(ptr addrspace(1) %obj)` operand bundle, got:\n{rewritten}"
  );

  // ...and thus a relocate for %obj must exist.
  assert!(
    rewritten.contains("@llvm.experimental.gc.relocate.p1"),
    "expected gc.relocate for %obj in rewritten IR, got:\n{rewritten}"
  );
}
