#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use inkwell::context::AsContextRef as _;
use inkwell::context::Context;
use inkwell::types::AsTypeRef as _;
use inkwell::values::AsValueRef as _;
use inkwell::AddressSpace;
use inkwell::OptimizationLevel;
use llvm_sys::core::{
  LLVMBuildGEP2, LLVMBuildRetVoid, LLVMBuildStore, LLVMGetInsertBlock, LLVMSetVolatile,
};
use native_js::emit::{emit_object, TargetConfig};
use native_js::gc::roots::GcFrame;
use native_js::gc::statepoint::StatepointEmitter;
use native_js::llvm::gc;
use object::{Object as _, ObjectSection as _};
use runtime_native::stackmaps::{parse_all_stackmaps, Location};
use runtime_native::statepoint_verify::{
  verify_statepoint_stackmap, DwarfArch, VerifyMode, VerifyStatepointOptions,
};
use runtime_native::statepoints::StatepointRecord;

#[test]
fn stackmaps_contain_derived_root_pairs_and_runtime_relocates_them() {
  let context = Context::create();
  let module = context.create_module("stackmaps_derived_pairs");
  let builder = context.create_builder();

  let void_ty = context.void_type();
  let i8_ty = context.i8_type();
  let i64_ty = context.i64_type();
  let gc_ptr_ty = context.ptr_type(AddressSpace::from(1u16));

  // declare void @callee()
  let callee_ty = void_ty.fn_type(&[], false);
  let callee = module.add_function("callee", callee_ty, None);

  // Volatile sinks to keep rooted values observable after the safepoint.
  let sink_base = module.add_global(gc_ptr_ty, None, "sink_base");
  sink_base.set_initializer(&gc_ptr_ty.const_null());
  let sink_derived = module.add_global(gc_ptr_ty, None, "sink_derived");
  sink_derived.set_initializer(&gc_ptr_ty.const_null());

  // define void @test(ptr addrspace(1) %base) gc "coreclr"
  let test_ty = void_ty.fn_type(&[gc_ptr_ty.into()], false);
  let test = module.add_function("test", test_ty, None);
  gc::set_default_gc_strategy(&test).expect("GC strategy contains NUL byte");
  let base = test
    .get_nth_param(0)
    .expect("missing base param")
    .into_pointer_value();
  base.set_name("base");

  let entry = context.append_basic_block(test, "entry");
  builder.position_at_end(entry);

  unsafe {
    let builder_ref = builder.as_mut_ptr();
    let entry_block = LLVMGetInsertBlock(builder_ref);
    let frame = GcFrame::new((&context).as_ctx_ref(), entry_block);
    let mut statepoints =
      StatepointEmitter::new((&context).as_ctx_ref(), module.as_mut_ptr(), frame.gc_ptr_ty());

    // Root the base.
    let base_slot = frame.root_base(builder_ref, base.as_value_ref());

    // Form and root an interior pointer: `derived = gep i8, base, 16`.
    let mut idx = [i64_ty.const_int(16, false).as_value_ref()];
    let derived = LLVMBuildGEP2(
      builder_ref,
      i8_ty.as_type_ref(),
      base.as_value_ref(),
      idx.as_mut_ptr(),
      idx.len() as u32,
      b"derived\0".as_ptr().cast(),
    );
    let derived_slot = frame.root_derived(builder_ref, &base_slot, derived);

    // Emit exactly one statepointed call.
    frame.safepoint_call(builder_ref, &mut statepoints, callee.as_value_ref(), &[]);

    // Force the relocated values to remain materialized by reading the slots after the safepoint and
    // storing them to volatile globals.
    let base_after = frame.load(builder_ref, base_slot, "base_after");
    let derived_after = frame.load(builder_ref, derived_slot, "derived_after");

    let base_store = LLVMBuildStore(
      builder_ref,
      base_after,
      sink_base.as_pointer_value().as_value_ref(),
    );
    LLVMSetVolatile(base_store, 1);

    let derived_store = LLVMBuildStore(
      builder_ref,
      derived_after,
      sink_derived.as_pointer_value().as_value_ref(),
    );
    LLVMSetVolatile(derived_store, 1);

    LLVMBuildRetVoid(builder_ref);
  }

  if let Err(err) = module.verify() {
    panic!(
      "module verification failed: {err}\n\nIR:\n{}",
      module.print_to_string()
    );
  }

  // Emit object + parse `.llvm_stackmaps`.
  let mut target = TargetConfig::default();
  target.cpu = "generic".to_string();
  target.features = "".to_string();
  target.opt_level = OptimizationLevel::None;
  let obj = emit_object(&module, target);

  let file = object::File::parse(&*obj).expect("parse object file");
  let section = file
    .section_by_name(".llvm_stackmaps")
    .expect("object missing .llvm_stackmaps section");
  let stackmaps_bytes = section.data().expect("read .llvm_stackmaps section");

  let stackmaps = parse_all_stackmaps(stackmaps_bytes).expect("parse stackmaps v3 blob(s)");
  assert!(
    !stackmaps.is_empty(),
    "expected at least one stackmap blob in .llvm_stackmaps"
  );

  // Preferred: run the runtime verifier to lock down invariants about statepoint records.
  for sm in &stackmaps {
    verify_statepoint_stackmap(
      sm,
      VerifyStatepointOptions {
        arch: DwarfArch::X86_64,
        mode: VerifyMode::StatepointsOnly,
      },
    )
    .expect("verify_statepoint_stackmap (StatepointsOnly)");
  }

  // Find a statepoint record containing at least one base!=derived pair, and extract the stack slot
  // offsets so we can smoke-test the runtime relocation logic.
  let mut found: Option<(i32, i32)> = None;
  let mut saw_statepoint = false;

  for sm in &stackmaps {
    for rec in &sm.records {
      let Ok(sp) = StatepointRecord::new(rec) else {
        continue;
      };
      saw_statepoint = true;
      assert!(
        sp.gc_pair_count() >= 2,
        "expected >=2 GC relocation pairs (base + derived), got {} (patchpoint_id=0x{:x})",
        sp.gc_pair_count(),
        rec.patchpoint_id
      );

      for pair in sp.gc_pairs() {
        if pair.base == pair.derived {
          continue;
        }

        let (Location::Indirect { dwarf_reg: base_reg, offset: base_off, .. }, Location::Indirect { dwarf_reg: derived_reg, offset: derived_off, .. }) =
          (&pair.base, &pair.derived)
        else {
          panic!(
            "expected derived relocation pair to use Indirect stack slots, got base={:?} derived={:?} (patchpoint_id=0x{:x})",
            pair.base, pair.derived, rec.patchpoint_id
          );
        };

        assert_eq!(
          *base_reg, *derived_reg,
          "expected base/derived to use the same base register (patchpoint_id=0x{:x})",
          rec.patchpoint_id
        );
        assert_ne!(
          *base_off, *derived_off,
          "expected base/derived stack slots to have distinct offsets (patchpoint_id=0x{:x})",
          rec.patchpoint_id
        );

        found = Some((*base_off, *derived_off));
        break;
      }
    }
  }

  assert!(saw_statepoint, "expected at least one statepoint record in stackmaps");
  let (base_off, derived_off) =
    found.expect("expected at least one base!=derived relocation pair in .llvm_stackmaps");

  // Runtime relocation smoke check: interpret the extracted Indirect offsets as stack slots in a
  // fake stack frame and ensure `relocate_derived_pairs` preserves the interior offset (16 bytes).
  assert_eq!(
    base_off % (std::mem::size_of::<usize>() as i32),
    0,
    "expected base slot offset to be pointer-aligned"
  );
  assert_eq!(
    derived_off % (std::mem::size_of::<usize>() as i32),
    0,
    "expected derived slot offset to be pointer-aligned"
  );

  let min_off = base_off.min(derived_off) as i64;
  let max_off = base_off.max(derived_off) as i64;
  let slot_size = std::mem::size_of::<usize>() as i64;
  let guard: i64 = 128;

  // Place our fake SP so `[SP + min_off, SP + max_off + slot_size)` fits inside the buffer.
  let before = guard + (-min_off).max(0);
  let after = guard + max_off.max(0) + slot_size;
  assert_eq!(
    before % slot_size,
    0,
    "test bug: computed SP base is misaligned"
  );

  let frame_bytes = (before + after) as usize;
  let words = (frame_bytes + std::mem::size_of::<usize>() - 1) / std::mem::size_of::<usize>();
  let mut frame: Vec<usize> = vec![0; words + 8];
  let frame_ptr = frame.as_mut_ptr().cast::<u8>();
  let sp = unsafe { frame_ptr.add(before as usize) };

  let base_slot = unsafe { sp.offset(base_off as isize).cast::<usize>() };
  let derived_slot = unsafe { sp.offset(derived_off as isize).cast::<usize>() };

  let base_old: usize = 0x1000;
  let derived_old: usize = base_old + 16;
  unsafe {
    base_slot.write_unaligned(base_old);
    derived_slot.write_unaligned(derived_old);
  }

  runtime_native::relocate_derived_pairs(&[(base_slot, derived_slot)], |p| p + 0x100);

  let base_new = unsafe { base_slot.read_unaligned() };
  let derived_new = unsafe { derived_slot.read_unaligned() };
  assert_eq!(base_new, 0x1100);
  assert_eq!(derived_new, 0x1100 + 16);
}

