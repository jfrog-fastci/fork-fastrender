use inkwell::context::Context;
use inkwell::memory_buffer::MemoryBuffer;
use inkwell::targets::{CodeModel, RelocMode, Target, TargetMachine};
use inkwell::OptimizationLevel;

use native_js::llvm::passes;

fn host_target_machine() -> TargetMachine {
  native_js::llvm::init_native_target().expect("failed to initialize native LLVM target");

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

#[test]
fn keep_alive_keeps_owner_in_statepoint_live_set() {
  // Derive a raw backing-store pointer from a GC-managed object, cross a call that will become a
  // statepoint, then use the raw pointer.
  //
  // The `rt_keep_alive_gc_ref_gc` call ensures `%owner` is considered live across the safepoint so
  // `rewrite-statepoints-for-gc` includes it in the `"gc-live"` list.
  let before = r#"
declare void @may_gc()
declare void @rt_keep_alive_gc_ref_gc(ptr addrspace(1)) #0

define void @test(ptr addrspace(1) %owner) gc "coreclr" {
entry:
  %field = getelementptr i8, ptr addrspace(1) %owner, i64 8
  %bs = load ptr, ptr addrspace(1) %field
  call void @may_gc()
  %byte = load i8, ptr %bs
  call void @rt_keep_alive_gc_ref_gc(ptr addrspace(1) %owner)
  ret void
}

attributes #0 = { "gc-leaf-function" }
"#;

  let context = Context::create();
  let buffer = MemoryBuffer::create_from_memory_range_copy(before.as_bytes(), "keep_alive.ll");
  let module = context
    .create_module_from_ir(buffer)
    .unwrap_or_else(|err| panic!("failed to parse LLVM IR: {err}\n\nIR:\n{before}"));

  let tm = host_target_machine();
  module.set_triple(&tm.get_triple());
  module.set_data_layout(&tm.get_target_data().get_data_layout());

  passes::rewrite_statepoints_for_gc(&module, &tm).unwrap_or_else(|err| {
    panic!(
      "failed to run rewrite-statepoints-for-gc: {err}\n\nBefore:\n{before}\n\nAfter:\n{}",
      module.print_to_string()
    )
  });

  let after = module.print_to_string().to_string();
  let func = function_block(&after, "@test");

  let statepoint_line = func
    .lines()
    .find(|l| l.contains("@may_gc"))
    .unwrap_or_else(|| panic!("missing @may_gc statepoint call in function:\n{func}"));

  assert!(
    statepoint_line.contains("\"gc-live\"(ptr addrspace(1) %owner)"),
    "expected %owner in gc-live list:\n{statepoint_line}\n\n{func}"
  );
  assert!(
    !statepoint_line.contains("%bs"),
    "raw backing-store pointer must not be treated as gc-live:\n{statepoint_line}\n\n{func}"
  );

  // KeepAlive should be after the final raw-pointer use.
  let raw_use = func
    .find("load i8, ptr %bs")
    .unwrap_or_else(|| panic!("expected raw pointer use in function IR:\n{func}"));
  let keep_alive = func
    .find("call void @rt_keep_alive_gc_ref_gc")
    .unwrap_or_else(|| panic!("expected keep-alive call in function IR:\n{func}"));
  assert!(
    raw_use < keep_alive,
    "keep-alive must be after last raw pointer use:\n{func}"
  );
}

#[test]
fn without_keep_alive_owner_is_not_in_statepoint_live_set() {
  // Same setup as `keep_alive_keeps_owner_in_statepoint_live_set`, but **without** a keep-alive call.
  //
  // This demonstrates the underlying hazard: because `%owner` is not used after the safepoint, LLVM
  // considers it dead and will not include it in the `"gc-live"` root list, even though we continue
  // using a raw pointer derived from it.
  let before = r#"
declare void @may_gc()

define void @test(ptr addrspace(1) %owner) gc "coreclr" {
entry:
  %field = getelementptr i8, ptr addrspace(1) %owner, i64 8
  %bs = load ptr, ptr addrspace(1) %field
  call void @may_gc()
  %byte = load i8, ptr %bs
  ret void
}
"#;

  let context = Context::create();
  let buffer =
    MemoryBuffer::create_from_memory_range_copy(before.as_bytes(), "keep_alive_missing.ll");
  let module = context
    .create_module_from_ir(buffer)
    .unwrap_or_else(|err| panic!("failed to parse LLVM IR: {err}\n\nIR:\n{before}"));

  let tm = host_target_machine();
  module.set_triple(&tm.get_triple());
  module.set_data_layout(&tm.get_target_data().get_data_layout());

  passes::rewrite_statepoints_for_gc(&module, &tm).unwrap_or_else(|err| {
    panic!(
      "failed to run rewrite-statepoints-for-gc: {err}\n\nBefore:\n{before}\n\nAfter:\n{}",
      module.print_to_string()
    )
  });

  let after = module.print_to_string().to_string();
  let func = function_block(&after, "@test");

  let statepoint_line = func
    .lines()
    .find(|l| l.contains("@may_gc"))
    .unwrap_or_else(|| panic!("missing @may_gc statepoint call in function:\n{func}"));

  assert!(
    !statepoint_line.contains("%owner"),
    "did not expect %owner to be live across the safepoint without a keep-alive:\n{statepoint_line}\n\n{func}"
  );
  assert!(
    !statepoint_line.contains("%bs"),
    "raw backing-store pointer must not be treated as gc-live:\n{statepoint_line}\n\n{func}"
  );
}

