use inkwell::context::Context;
use inkwell::memory_buffer::MemoryBuffer;
use inkwell::targets::{CodeModel, InitializationConfig, RelocMode, Target, TargetMachine};
use inkwell::OptimizationLevel;
use native_js::llvm::passes;
use std::sync::Once;

static LLVM_INIT: Once = Once::new();

fn init_llvm() {
  LLVM_INIT.call_once(|| {
    Target::initialize_native(&InitializationConfig::default()).expect("failed to initialize native target");
  });
}

fn host_target_machine() -> TargetMachine {
  init_llvm();

  let triple = TargetMachine::get_default_triple();
  let target = Target::from_triple(&triple).expect("host target");
  let cpu = TargetMachine::get_host_cpu_name().to_string();
  let features = TargetMachine::get_host_cpu_features().to_string();

  target
    .create_target_machine(
      &triple,
      &cpu,
      &features,
      OptimizationLevel::None,
      RelocMode::Default,
      CodeModel::Default,
    )
    .expect("create target machine")
}

#[test]
fn place_safepoints_inserts_poll_calls_and_rewrites_them_to_statepoints() {
  let before = r#"
source_filename = "place_safepoints_test"

define void @test(ptr addrspace(1) %obj, i64 %n) gc "coreclr" {
entry:
  br label %loop

loop:
  %i = phi i64 [ 0, %entry ], [ %i.next, %loop ]
  %p = getelementptr i8, ptr addrspace(1) %obj, i64 %i
  %v = load i8, ptr addrspace(1) %p, align 1
  %i.next = add i64 %i, 1
  %cond = icmp ult i64 %i.next, %n
  br i1 %cond, label %loop, label %exit

exit:
  %q = getelementptr i8, ptr addrspace(1) %obj, i64 0
  store i8 %v, ptr addrspace(1) %q, align 1
  ret void
}
"#;

  init_llvm();

  let context = Context::create();
  let buffer = MemoryBuffer::create_from_memory_range_copy(before.as_bytes(), "test.ll");
  let module = context
    .create_module_from_ir(buffer)
    .unwrap_or_else(|err| panic!("failed to parse LLVM IR: {err}\n\nIR:\n{before}"));

  let tm = host_target_machine();
  module.set_triple(&tm.get_triple());
  module.set_data_layout(&tm.get_target_data().get_data_layout());

  passes::place_safepoints_and_rewrite_for_gc(&module, &tm).expect("place-safepoints pipeline failed");

  if let Err(err) = module.verify() {
    panic!(
      "module verification failed after place-safepoints + rewrite-statepoints-for-gc: {err}\n\nIR:\n{}",
      module.print_to_string()
    );
  }

  let after = module.print_to_string().to_string();

  assert!(
    after.contains("declare void @gc.safepoint_poll()"),
    "expected gc.safepoint_poll to be predeclared (LLVM 18 place-safepoints workaround):\n{after}"
  );

  let statepoint_polls = after
    .lines()
    .filter(|l| l.contains("@llvm.experimental.gc.statepoint") && l.contains("@gc.safepoint_poll"))
    .count();

  assert!(
    statepoint_polls >= 2,
    "expected >=2 statepoints that call gc.safepoint_poll (entry + backedge), got {statepoint_polls}:\n{after}"
  );
}

