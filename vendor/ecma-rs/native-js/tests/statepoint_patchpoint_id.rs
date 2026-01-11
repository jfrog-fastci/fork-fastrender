#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use inkwell::context::AsContextRef as _;
use inkwell::context::Context;
use inkwell::targets::{CodeModel, FileType, RelocMode, Target, TargetMachine};
use inkwell::types::AsTypeRef as _;
use inkwell::values::AsValueRef as _;
use inkwell::OptimizationLevel;
use native_js::gc::statepoint::StatepointEmitter;
use native_js::gc::statepoints::{StatepointCallee, StatepointIntrinsics};
use native_js::llvm::gc;
use object::{Object, ObjectSection};
use runtime_native::stackmaps::StackMap;
use runtime_native::statepoint_verify::{
  verify_statepoint_stackmap, DwarfArch, VerifyMode, VerifyStatepointOptions,
  LLVM_STATEPOINT_PATCHPOINT_ID,
};
use tempfile::tempdir;

const NUM_STATEPOINTS: usize = 3;

fn host_target_machine() -> TargetMachine {
  native_js::llvm::init_native_target().expect("failed to init native target");

  let triple = TargetMachine::get_default_triple();
  let target = Target::from_triple(&triple).expect("host target");
  target
    .create_target_machine(
      &triple,
      "generic",
      "",
      OptimizationLevel::None,
      RelocMode::Default,
      CodeModel::Default,
    )
    .expect("create target machine")
}

fn emit_object_and_parse_stackmap(module: &inkwell::module::Module<'_>, tm: &TargetMachine) -> StackMap {
  module.set_triple(&tm.get_triple());
  module.set_data_layout(&tm.get_target_data().get_data_layout());

  let tmp = tempdir().expect("failed to create tempdir");
  let obj = tmp.path().join("m.o");
  tm.write_to_file(module, FileType::Object, &obj)
    .expect("failed to emit object file");

  let bytes = std::fs::read(&obj).expect("read object file");
  let file = object::File::parse(&*bytes).expect("parse object file");
  let section = file
    .section_by_name(".llvm_stackmaps")
    .expect("expected .llvm_stackmaps section");
  StackMap::parse(section.data().expect("read .llvm_stackmaps"))
    .expect("parse stackmap v3")
}

fn assert_statepoints_are_canonical_and_verified(stackmap: &StackMap) {
  assert_eq!(
    stackmap.records.len(),
    NUM_STATEPOINTS,
    "expected exactly {NUM_STATEPOINTS} stackmap records, got {}\n\nstackmap={stackmap:?}",
    stackmap.records.len()
  );

  for (idx, rec) in stackmap.records.iter().enumerate() {
    assert_eq!(
      rec.patchpoint_id, LLVM_STATEPOINT_PATCHPOINT_ID,
      "expected record[{idx}] patchpoint_id to be LLVM_STATEPOINT_PATCHPOINT_ID (0xABCDEF00), got 0x{:x}",
      rec.patchpoint_id
    );
  }

  verify_statepoint_stackmap(
    stackmap,
    VerifyStatepointOptions {
      arch: DwarfArch::X86_64,
      mode: VerifyMode::StatepointsOnly,
    },
  )
  .expect("verify_statepoint_stackmap (StatepointsOnly)");
}

#[test]
fn statepoint_emitter_uses_canonical_patchpoint_id() {
  let context = Context::create();
  let module = context.create_module("statepoint_emitter_patchpoint_id");
  let builder = context.create_builder();

  let void_ty = context.void_type();
  let gc_ptr_ty = gc::gc_ptr_type(&context);

  // declare void @callee()
  let callee_ty = void_ty.fn_type(&[], false);
  let callee = module.add_function("callee", callee_ty, None);

  // define void @caller() gc "coreclr"
  let caller_ty = void_ty.fn_type(&[], false);
  let caller = module.add_function("caller", caller_ty, None);
  gc::set_default_gc_strategy(&caller).expect("GC strategy contains NUL byte");

  let entry = context.append_basic_block(caller, "entry");
  builder.position_at_end(entry);

  unsafe {
    let mut sp = StatepointEmitter::new(
      (&context).as_ctx_ref(),
      module.as_mut_ptr(),
      gc_ptr_ty.as_type_ref(),
    );
    for _ in 0..NUM_STATEPOINTS {
      let _ = sp.emit_statepoint_call(builder.as_mut_ptr(), callee.as_value_ref(), &[], &[]);
    }
  }
  builder.build_return(None).expect("build return");

  if let Err(err) = module.verify() {
    panic!(
      "module verification failed: {err}\n\nIR:\n{}",
      module.print_to_string()
    );
  }

  let tm = host_target_machine();
  let stackmap = emit_object_and_parse_stackmap(&module, &tm);
  assert_statepoints_are_canonical_and_verified(&stackmap);
}

#[test]
fn statepoint_intrinsics_use_canonical_patchpoint_id() {
  let context = Context::create();
  let module = context.create_module("statepoint_intrinsics_patchpoint_id");
  let builder = context.create_builder();

  let void_ty = context.void_type();

  // declare void @callee()
  let callee_ty = void_ty.fn_type(&[], false);
  let callee = module.add_function("callee", callee_ty, None);

  // define void @caller() gc "coreclr"
  let caller_ty = void_ty.fn_type(&[], false);
  let caller = module.add_function("caller", caller_ty, None);
  gc::set_default_gc_strategy(&caller).expect("GC strategy contains NUL byte");

  let entry = context.append_basic_block(caller, "entry");
  builder.position_at_end(entry);

  let mut intrinsics = StatepointIntrinsics::new(&module);
  for _ in 0..NUM_STATEPOINTS {
    let _ = intrinsics.emit_statepoint_call(
      &builder,
      StatepointCallee::new(callee.as_global_value().as_pointer_value(), callee_ty),
      &[],
      &[],
      None,
    );
  }
  builder.build_return(None).expect("build return");

  if let Err(err) = module.verify() {
    panic!(
      "module verification failed: {err}\n\nIR:\n{}",
      module.print_to_string()
    );
  }

  let tm = host_target_machine();
  let stackmap = emit_object_and_parse_stackmap(&module, &tm);
  assert_statepoints_are_canonical_and_verified(&stackmap);
}

