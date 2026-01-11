use inkwell::context::Context;
use inkwell::targets::{CodeModel, RelocMode, Target, TargetMachine};
use inkwell::OptimizationLevel;
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

fn assigned_ssa(line: &str) -> Option<String> {
  let (lhs, _rhs) = line.split_once('=')?;
  Some(lhs.trim().to_string())
}

#[test]
fn runtime_handle_abi_uses_pointer_to_slot_at_statepoint() {
  native_js::llvm::init_native_target().expect("failed to initialize native LLVM target");

  let context = Context::create();
  let module = context.create_module("runtime_handle_abi");
  let builder = context.create_builder();

  let runtime_abi = RuntimeAbi::new(&context, &module);
  runtime_abi.ensure_wrappers();

  // define ptr addrspace(1) @test(ptr addrspace(1) %p) gc "coreclr"
  let gc_ptr_ty = gc::gc_ptr_type(&context);
  let test_ty = gc_ptr_ty.fn_type(&[gc_ptr_ty.into()], false);
  let test_fn = module.add_function("test", test_ty, None);
  gc::set_default_gc_strategy(&test_fn).expect("GC strategy contains NUL byte");

  let entry = context.append_basic_block(test_fn, "entry");
  builder.position_at_end(entry);

  let p = test_fn
    .get_first_param()
    .expect("missing %p param")
    .into_pointer_value();
  p.set_name("p");

  // Root %p in an address-taken stack slot so we can pass `&slot` as a `GcHandle`.
  let slot = builder
    .build_alloca(gc_ptr_ty, "slot")
    .expect("alloca gc root slot");
  builder.build_store(slot, p).expect("store p to slot");

  // Load the pointer before the call and use it after the call so it is live across the safepoint.
  let live = builder
    .build_load(gc_ptr_ty, slot, "live")
    .expect("load live")
    .into_pointer_value();

  // Emit a call to the handle-based runtime entrypoint. ABI validation should accept this even
  // though the runtime may GC, because the argument is a handle (`ptr %slot`), not a raw
  // `ptr addrspace(1)` GC pointer.
  let call = runtime_abi
    .emit_runtime_call(
      &builder,
      RuntimeFn::GcSafepointRelocateH,
      &[slot.into()],
      "reloc",
    )
    .expect("emit handle-based runtime call");
  let relocated = call
    .try_as_basic_value()
    .left()
    .expect("rt_gc_safepoint_relocate_h returns a value")
    .into_pointer_value();

  // Store the live pointer back into the slot after the call. After statepoint rewriting, this
  // store must use the `gc.relocate`d value.
  builder
    .build_store(slot, live)
    .expect("write back relocated live pointer");
  builder
    .build_is_null(live, "isnull")
    .expect("use live after call");

  builder
    .build_return(Some(&relocated))
    .expect("return relocated pointer");

  if let Err(err) = module.verify() {
    panic!(
      "module verification failed before rewrite-statepoints-for-gc: {err}\n\nIR:\n{}",
      module.print_to_string()
    );
  }

  // Rewrite calls into gc.statepoint intrinsics and materialize gc.relocate/gc.result.
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

  // The call must be rewritten to a statepoint and must pass `ptr %slot` (pointer-to-slot handle)
  // as its call argument.
  let statepoint_line = func
    .lines()
    .find(|l| l.contains("@llvm.experimental.gc.statepoint") && l.contains("rt_gc_safepoint_relocate_h"))
    .unwrap_or_else(|| panic!("missing statepointed call to relocate helper in:\n{func}"));
  assert!(
    statepoint_line.contains("ptr %slot"),
    "expected statepoint call to pass handle argument `ptr %slot`, got:\n{statepoint_line}\n\n{func}"
  );
  assert!(
    !statepoint_line.contains("ptr addrspace(1) %slot"),
    "handle argument must be a pointer-to-slot (addrspace(0) `ptr`), not a GC pointer:\n{statepoint_line}\n\n{func}"
  );

  // The GC pointer loaded from the slot must be present in the gc-live bundle so it is relocated
  // across the safepoint.
  assert!(
    statepoint_line.contains("\"gc-live\"(ptr addrspace(1) %live)"),
    "expected gc-live bundle to include %live:\n{statepoint_line}\n\n{func}"
  );

  // The post-call store back into %slot must use the relocated SSA value.
  let relocate_line = func
    .lines()
    .find(|l| l.contains("@llvm.experimental.gc.relocate.p1"))
    .unwrap_or_else(|| panic!("missing gc.relocate.p1 in:\n{func}"));
  let relocated_live = assigned_ssa(relocate_line)
    .unwrap_or_else(|| panic!("expected gc.relocate to assign to an SSA value: {relocate_line}"));
  assert!(
    func.contains(&format!("store ptr addrspace(1) {relocated_live}, ptr %slot")),
    "expected relocated live value {relocated_live} to be stored back into %slot:\n{func}"
  );

  // The runtime call returns a GC-managed pointer and should be recovered via gc.result.p1.
  assert!(
    func.contains("@llvm.experimental.gc.result.p1"),
    "expected gc.result.p1 for ptr addrspace(1) return:\n{func}"
  );
  assert!(
    func.contains("ret ptr addrspace(1) %"),
    "expected function to return a ptr addrspace(1):\n{func}"
  );
}
