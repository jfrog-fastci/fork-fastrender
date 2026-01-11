#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use inkwell::context::AsContextRef as _;
use inkwell::context::Context;
use inkwell::types::AsTypeRef as _;
use inkwell::values::AsValueRef as _;
use inkwell::AddressSpace;
use llvm_sys::core::{LLVMBuildGEP2, LLVMBuildPtrToInt, LLVMBuildRet, LLVMGetInsertBlock};
use native_js::emit::{emit_object, TargetConfig};
use native_js::gc::roots::GcFrame;
use native_js::gc::statepoint::StatepointEmitter;
use native_js::llvm::gc as llvm_gc;
use native_js::runtime_fn::RuntimeFn;
use object::{Object as _, ObjectSection as _};
use runtime_native::stackmaps::{parse_all_stackmaps, Location};
use runtime_native::statepoints::StatepointRecord;
use std::ffi::CString;

#[test]
fn derived_pointer_statepoint_emits_base_derived_stackmap_pair() {
  let context = Context::create();
  let module = context.create_module("derived_pointer_statepoint");
  let builder = context.create_builder();

  let void_ty = context.void_type();
  let i64_ty = context.i64_type();
  let i32_ty = context.i32_type();
  let i8_ty = context.i8_type();
  let gc_ptr_ty = context.ptr_type(AddressSpace::from(1u16));

  // declare ptr addrspace(1) @rt_alloc(i64, i32)
  let rt_alloc_ty = gc_ptr_ty.fn_type(&[i64_ty.into(), i32_ty.into()], false);
  let rt_alloc = module.add_function(RuntimeFn::Alloc.llvm_name(), rt_alloc_ty, None);

  // declare void @may_gc()
  let may_gc_ty = void_ty.fn_type(&[], false);
  let may_gc = module.add_function("may_gc", may_gc_ty, None);

  // define i64 @test() gc "coreclr"
  let test_ty = i64_ty.fn_type(&[], false);
  let test_fn = module.add_function("test", test_ty, None);
  llvm_gc::set_default_gc_strategy(&test_fn).expect("GC strategy contains NUL byte");

  let entry = context.append_basic_block(test_fn, "entry");
  builder.position_at_end(entry);

  unsafe {
    let builder_ref = builder.as_mut_ptr();
    let entry_block = LLVMGetInsertBlock(builder_ref);
    let frame = GcFrame::new((&context).as_ctx_ref(), entry_block);
    let mut statepoints =
      StatepointEmitter::new((&context).as_ctx_ref(), module.as_mut_ptr(), frame.gc_ptr_ty());

    // Allocate an object pointer to use as the base.
    let size = i64_ty.const_int(32, false).as_value_ref();
    let shape = i32_ty.const_int(1, false).as_value_ref();
    let base = frame
      .safepoint_call(builder_ref, &mut statepoints, rt_alloc.as_value_ref(), &[size, shape])
      .expect("rt_alloc returns ptr addrspace(1)");

    // Root the base pointer and also root a derived/interior pointer.
    let base_slot = frame.root_base(builder_ref, base);

    let offset = i64_ty.const_int(8, false).as_value_ref();
    let mut idxs = [offset];
    let derived = LLVMBuildGEP2(
      builder_ref,
      i8_ty.as_type_ref(),
      base,
      idxs.as_mut_ptr(),
      idxs.len() as u32,
      CString::new("derived").unwrap().as_ptr(),
    );
    let derived_slot = frame.root_derived(builder_ref, &base_slot, derived);

    // Emit a safepoint with the derived pointer live across it.
    frame.safepoint_call(builder_ref, &mut statepoints, may_gc.as_value_ref(), &[]);

    // Use both pointers after the safepoint so the relocated slot writeback can't be DCE'd during
    // CodeGen.
    let base_after = frame.load(builder_ref, base_slot, "base_after");
    let derived_after = frame.load(builder_ref, derived_slot, "derived_after");

    let base_i64 = LLVMBuildPtrToInt(
      builder_ref,
      base_after,
      i64_ty.as_type_ref(),
      b"base_i64\0".as_ptr().cast(),
    );
    let derived_i64 = LLVMBuildPtrToInt(
      builder_ref,
      derived_after,
      i64_ty.as_type_ref(),
      b"derived_i64\0".as_ptr().cast(),
    );
    let sum = llvm_sys::core::LLVMBuildAdd(
      builder_ref,
      base_i64,
      derived_i64,
      b"sum\0".as_ptr().cast(),
    );

    LLVMBuildRet(builder_ref, sum);
  }

  if let Err(err) = module.verify() {
    panic!(
      "module verification failed: {err}\n\nIR:\n{}",
      module.print_to_string()
    );
  }
  let ir = module.print_to_string().to_string();

  let mut target = TargetConfig::default();
  target.opt_level = inkwell::OptimizationLevel::None;
  let obj = emit_object(&module, target);

  let file = object::File::parse(&*obj).expect("parse object file");
  let stackmaps = file
    .section_by_name(".llvm_stackmaps")
    .expect("missing .llvm_stackmaps section");
  let data = stackmaps.data().expect("read .llvm_stackmaps");
  let stackmaps = parse_all_stackmaps(data).expect("parse stackmaps");

  let mut saw_statepoint = false;
  let mut saw_statepoint_with_roots = false;
  let mut saw_derived_pair = false;
  for map in &stackmaps {
    for rec in &map.records {
      let Ok(sp) = StatepointRecord::new(rec) else {
        continue;
      };
      saw_statepoint = true;

      // A statepoint can legally have 0 gc-live values (e.g. an allocating callsite that occurs
      // before any roots are established). We only require that *some* statepoint record has roots,
      // and that at least one of those roots is a true derived pointer (base != derived).
      if sp.gc_pair_count() == 0 {
        continue;
      }
      saw_statepoint_with_roots = true;

      for pair in sp.gc_pairs() {
        if pair.base == pair.derived {
          continue;
        }

        // Lock in that LLVM is emitting a true base!=derived pair (interior pointer) as distinct
        // stack slots.
        match (&pair.base, &pair.derived) {
          (
            Location::Indirect {
              dwarf_reg: base_reg,
              offset: base_off,
              ..
            },
            Location::Indirect {
              dwarf_reg: derived_reg,
              offset: derived_off,
              ..
            },
          ) => {
            assert_eq!(
              base_reg, derived_reg,
              "expected base+derived to be addressed off the same base register\n\npair={pair:?}\n\nIR:\n{ir}"
            );
            assert_ne!(
              base_off, derived_off,
              "expected base and derived pointers to live in distinct spill slots\n\npair={pair:?}\n\nIR:\n{ir}"
            );
            saw_derived_pair = true;
          }
          other => {
            panic!(
              "expected interior pointer pair to be encoded as Indirect stack slots, got {other:?}\n\npair={pair:?}\n\nrecord={rec:?}\n\nIR:\n{ir}"
            );
          }
        }
      }
    }
  }

  assert!(
    saw_statepoint,
    "expected at least one LLVM statepoint record in stackmaps\n\nIR:\n{ir}"
  );
  assert!(
    saw_statepoint_with_roots,
    "expected at least one LLVM statepoint record with GC roots (gc_pair_count > 0)\n\nstackmaps={stackmaps:?}\n\nIR:\n{ir}"
  );
  assert!(
    saw_derived_pair,
    "expected to find at least one (base, derived) pair with base != derived in stackmaps\n\nstackmaps={stackmaps:?}\n\nIR:\n{ir}"
  );
}
