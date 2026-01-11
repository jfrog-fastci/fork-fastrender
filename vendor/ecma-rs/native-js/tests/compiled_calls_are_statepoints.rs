use inkwell::context::AsContextRef as _;
use inkwell::context::Context;
use inkwell::values::AsValueRef as _;
use inkwell::AddressSpace;
use llvm_sys::core::{LLVMBuildRet, LLVMGetInsertBlock};
use native_js::emit::{emit_object, TargetConfig};
use native_js::gc::roots::GcFrame;
use native_js::gc::statepoint::StatepointEmitter;
use native_js::llvm::gc as llvm_gc;
use native_js::runtime_fn::RuntimeFn;
use object::{Object as _, ObjectSection as _};
use runtime_native::stackmaps::parse_all_stackmaps;

#[test]
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn compiled_calls_may_gc_are_emitted_as_statepoints() {
  let context = Context::create();
  let module = context.create_module("compiled_calls_are_statepoints");
  let builder = context.create_builder();

  let i64_ty = context.i64_type();
  let i32_ty = context.i32_type();
  let gc_ptr_ty = context.ptr_type(AddressSpace::from(1u16));

  // Declare a runtime allocation entrypoint. This is an extern in the real
  // system; for stackmap emission we only need the signature.
  let rt_alloc_ty = gc_ptr_ty.fn_type(&[i64_ty.into(), i32_ty.into()], false);
  let rt_alloc = module.add_function(RuntimeFn::Alloc.llvm_name(), rt_alloc_ty, None);

  // Callee: performs an allocating runtime call via a statepoint.
  let callee_ty = gc_ptr_ty.fn_type(&[gc_ptr_ty.into()], false);
  let callee = module.add_function("callee", callee_ty, None);
  llvm_gc::set_default_gc_strategy(&callee).expect("GC strategy contains NUL byte");

  let entry = context.append_basic_block(callee, "entry");
  builder.position_at_end(entry);

  unsafe {
    let builder_ref = builder.as_mut_ptr();
    let entry_block = LLVMGetInsertBlock(builder_ref);
    let frame = GcFrame::new((&context).as_ctx_ref(), entry_block);
    let mut statepoints =
      StatepointEmitter::new((&context).as_ctx_ref(), module.as_mut_ptr(), frame.gc_ptr_ty());

    let size = i64_ty.const_int(16, false).as_value_ref();
    let shape = i32_ty.const_int(1, false).as_value_ref();
    let allocated = frame
      .safepoint_call(builder_ref, &mut statepoints, rt_alloc.as_value_ref(), &[size, shape])
      .expect("rt_alloc returns ptr addrspace(1)");

    LLVMBuildRet(builder_ref, allocated);
  }

  // Caller: passes a GC pointer to `callee` and (conservatively) treats the call as may-GC, so the
  // callsite must be a statepoint.
  let caller_ty = gc_ptr_ty.fn_type(&[gc_ptr_ty.into()], false);
  let caller = module.add_function("caller", caller_ty, None);
  llvm_gc::set_default_gc_strategy(&caller).expect("GC strategy contains NUL byte");

  let entry = context.append_basic_block(caller, "entry");
  builder.position_at_end(entry);

  unsafe {
    let builder_ref = builder.as_mut_ptr();
    let entry_block = LLVMGetInsertBlock(builder_ref);
    let frame = GcFrame::new((&context).as_ctx_ref(), entry_block);
    let mut statepoints =
      StatepointEmitter::new((&context).as_ctx_ref(), module.as_mut_ptr(), frame.gc_ptr_ty());

    let arg = caller
      .get_nth_param(0)
      .expect("caller param")
      .into_pointer_value();
    arg.set_name("arg");

    // Root the arg so it is part of the current rooted set.
    let _ = frame.root_base(builder_ref, arg.as_value_ref());

    let callee_res = frame
      .compiled_call(
        builder_ref,
        &mut statepoints,
        callee.as_value_ref(),
        &[arg.as_value_ref()],
        None, // conservative default: assume may-GC
      )
      .expect("callee returns ptr addrspace(1)");

    LLVMBuildRet(builder_ref, callee_res);
  }

  if let Err(err) = module.verify() {
    panic!("module verification failed: {err}\n\nIR:\n{}", module.print_to_string());
  }

  let ir = module.print_to_string().to_string();

  // Ensure the caller->callee callsite is a statepoint (not a plain call).
  assert!(
    ir.contains("@llvm.experimental.gc.statepoint.p0") && ir.contains("elementtype(ptr addrspace(1) (ptr addrspace(1))) @callee"),
    "expected caller->callee statepoint in IR, got:\n{ir}"
  );

  // Ensure the call argument itself is tracked in gc-live (Task 331).
  assert!(
    ir.contains("\"gc-live\"") && ir.contains("ptr addrspace(1) %arg"),
    "expected call arg to appear in gc-live operand bundle, got:\n{ir}"
  );

  // Emit object and assert stackmaps contain both callsites:
  // - caller -> callee
  // - callee -> rt_alloc
  let obj = emit_object(&module, TargetConfig::default());
  let file = object::File::parse(&*obj).expect("parse object file");
  let stackmaps = file
    .section_by_name(".llvm_stackmaps")
    .expect("missing .llvm_stackmaps section");
  let data = stackmaps.data().expect("read .llvm_stackmaps section");
  let stackmaps = parse_all_stackmaps(data).expect("parse stackmaps section");
  let num_records: usize = stackmaps.iter().map(|map| map.records.len()).sum();
  assert!(
    num_records >= 2,
    "expected at least 2 stackmap records (caller->callee + callee->alloc), got {num_records} ({} stackmap blobs)",
    stackmaps.len(),
  );
}
