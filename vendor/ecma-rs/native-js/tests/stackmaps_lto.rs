#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use inkwell::context::Context;
use inkwell::targets::{CodeModel, RelocMode, Target, TargetMachine};
use inkwell::OptimizationLevel;
use native_js::link::{LLVM_STACKMAPS_START_SYM, LLVM_STACKMAPS_STOP_SYM, LinkOpts};
use native_js::llvm::{gc, passes};
use object::{Object, ObjectSection, ObjectSegment, ObjectSymbol, SymbolScope};
use runtime_native::stackmaps::StackMaps;
use runtime_native::statepoint_verify::{
  verify_statepoint_stackmap, DwarfArch, VerifyMode, VerifyStatepointOptions,
};
use std::process::Command;

fn has_clang_18() -> bool {
  Command::new("clang-18")
    .arg("--version")
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .status()
    .is_ok_and(|s| s.success())
}

fn find_symbol<'data>(file: &object::File<'data>, name: &str) -> Option<(u64, SymbolScope)> {
  for sym in file.symbols() {
    if sym.name().ok() == Some(name) {
      return Some((sym.address(), sym.scope()));
    }
  }
  for sym in file.dynamic_symbols() {
    if sym.name().ok() == Some(name) {
      return Some((sym.address(), sym.scope()));
    }
  }
  None
}

fn segment_is_readable(flags: object::SegmentFlags) -> bool {
  // PF_R on ELF is bit 2 (value 4).
  match flags {
    object::SegmentFlags::Elf { p_flags } => (p_flags & 4) != 0,
    _ => true,
  }
}

fn assert_stackmaps_present(exe: &[u8]) {
  let file = object::File::parse(exe).expect("parse ELF");
  let section = file
    .section_by_name(".data.rel.ro.llvm_stackmaps")
    .or_else(|| file.section_by_name(".llvm_stackmaps"))
    .expect("missing stackmaps section (was it GC'd?)");

  let section_addr = section.address();
  let section_size = section.size();
  assert!(section_size > 0, "expected non-empty stackmaps section");

  // Ensure statepoint roots are spilled to stack slots, not registers. LTO code
  // generation happens inside `clang-18`, so this guards against it accidentally
  // emitting `Register` root locations that our frame-pointer-only stack walker
  // can't reconstruct.
  let stackmaps_bytes = section.data().expect("read .llvm_stackmaps");
  let stackmaps = StackMaps::parse(stackmaps_bytes).expect("parse .llvm_stackmaps");
  for raw in stackmaps.raws() {
    verify_statepoint_stackmap(
      raw,
      VerifyStatepointOptions {
        arch: DwarfArch::X86_64,
        mode: VerifyMode::StatepointsOnly,
      },
    )
    .expect("statepoint stackmap verification failed");
  }

  let (start, start_scope) =
    find_symbol(&file, LLVM_STACKMAPS_START_SYM).expect("missing __start_llvm_stackmaps symbol");
  let (end, end_scope) =
    find_symbol(&file, LLVM_STACKMAPS_STOP_SYM).expect("missing __stop_llvm_stackmaps symbol");

  assert_ne!(
    start_scope,
    SymbolScope::Compilation,
    "{LLVM_STACKMAPS_START_SYM} must be globally linkable (not a local symbol)"
  );
  assert_ne!(
    end_scope,
    SymbolScope::Compilation,
    "{LLVM_STACKMAPS_STOP_SYM} must be globally linkable (not a local symbol)"
  );

  assert_eq!(
    start, section_addr,
    "start symbol must equal the stackmaps section virtual address"
  );
  assert_eq!(
    end.checked_sub(start).unwrap(),
    section_size,
    "end-start must equal the stackmaps section size"
  );

  // Optional: ensure the section is backed by a readable load segment so the runtime can read the
  // bytes directly from memory (via the start/end symbol pointers).
  let mut in_readable_segment = false;
  let section_end = section_addr + section_size;
  for seg in file.segments() {
    let seg_addr = seg.address();
    let seg_end = seg_addr + seg.size();
    let flags = seg.flags();
    if seg_addr <= section_addr && section_end <= seg_end && segment_is_readable(flags) {
      in_readable_segment = true;
      break;
    }
  }
  assert!(
    in_readable_segment,
    "stackmaps section not in a readable segment"
  );
}

#[test]
fn stackmaps_survive_lto_with_and_without_gc_sections() {
  if !has_clang_18() {
    eprintln!("skipping: clang-18 not found in PATH");
    return;
  }

  // Build the "statepoint PoC" module (same pattern as `statepoint_stackmap.rs`) and then exercise
  // the `clang-18 -flto` link path.
  native_js::llvm::init_native_target().expect("failed to init native target");

  let context = Context::create();
  let module = context.create_module("statepoints_lto");
  let builder = context.create_builder();

  let gc_ptr = gc::gc_ptr_type(&context);

  // declare void @callee()
  let callee_ty = context.void_type().fn_type(&[], false);
  let callee = module.add_function("callee", callee_ty, None);
  let callee_entry = context.append_basic_block(callee, "entry");
  builder.position_at_end(callee_entry);
  builder.build_return(None).unwrap();

  // define ptr addrspace(1) @test(ptr addrspace(1)) gc "coreclr"
  let test_ty = gc_ptr.fn_type(&[gc_ptr.into()], false);
  let test_fn = module.add_function("test", test_ty, None);
  gc::set_default_gc_strategy(&test_fn).expect("GC strategy contains NUL byte");

  let entry = context.append_basic_block(test_fn, "entry");
  builder.position_at_end(entry);

  // Ensure the GC pointer argument is live across the call.
  builder.build_call(callee, &[], "call_callee").unwrap();
  let arg0 = test_fn
    .get_first_param()
    .expect("missing arg0")
    .into_pointer_value();
  builder.build_return(Some(&arg0)).unwrap();

  // define i32 @main() { call @test(null); ret 0 }
  //
  // This keeps `@test` reachable under LTO so its safepoints/stackmaps survive internalization.
  let i32_ty = context.i32_type();
  let main_ty = i32_ty.fn_type(&[], false);
  let main_fn = module.add_function("main", main_ty, None);
  let main_entry = context.append_basic_block(main_fn, "entry");
  builder.position_at_end(main_entry);
  builder
    .build_call(test_fn, &[gc_ptr.const_null().into()], "call_test")
    .unwrap();
  builder
    .build_return(Some(&i32_ty.const_int(0, false)))
    .unwrap();

  let triple = TargetMachine::get_default_triple();
  let target = Target::from_triple(&triple).expect("no target for default triple");
  let tm = target
    .create_target_machine(
      &triple,
      "generic",
      "",
      OptimizationLevel::None,
      RelocMode::Default,
      CodeModel::Default,
    )
    .expect("failed to create target machine");

  native_js::llvm::apply_target_machine(&module, &tm);
  passes::rewrite_statepoints_for_gc(&module, &tm).expect("rewrite-statepoints-for-gc failed");

  let bitcode = native_js::llvm::emit_bitcode(&module, &tm);

  // Without `--gc-sections`.
  let exe = native_js::link::link_bitcode_to_exe(
    &bitcode,
    LinkOpts {
      gc_sections: false,
      ..Default::default()
    },
  )
  .expect("LTO link (no GC sections) failed");
  assert_stackmaps_present(&exe);

  // With `--gc-sections` (regression test for `.llvm_stackmaps` being GC'd under LTO).
  let exe_gc = native_js::link::link_bitcode_to_exe(
    &bitcode,
    LinkOpts {
      gc_sections: true,
      ..Default::default()
    },
  )
  .expect("LTO link (--gc-sections) failed");
  assert_stackmaps_present(&exe_gc);
}
