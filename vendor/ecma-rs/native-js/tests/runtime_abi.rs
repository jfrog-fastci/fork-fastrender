use inkwell::context::Context;
use inkwell::values::AsValueRef;
use llvm_sys::core::LLVMGetStringAttributeAtIndex;
use llvm_sys::LLVMAttributeFunctionIndex;
use inkwell::AddressSpace;
use native_js::emit::{emit_object_with_statepoints, TargetConfig};
use native_js::llvm::gc;
use native_js::runtime_abi::{RuntimeAbi, RuntimeFn};
use object::{Object as _, ObjectSection as _, ObjectSymbol as _, RelocationKind, RelocationTarget};
use runtime_native::statepoint_verify::{load_stackmap, DwarfArch};
use runtime_native::StackMaps;
use std::process::{Command, Stdio};

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

fn runtime_native_arch() -> Option<DwarfArch> {
  if cfg!(target_arch = "x86_64") {
    Some(DwarfArch::X86_64)
  } else if cfg!(target_arch = "aarch64") {
    Some(DwarfArch::AArch64)
  } else {
    None
  }
}

fn relocate_stackmaps_section<'data>(
  file: &object::File<'data>,
  section: &object::Section<'data, '_>,
) -> Vec<u8> {
  let mut data = section
    .data()
    .expect("read .llvm_stackmaps section")
    .to_vec();

  for (off, reloc) in section.relocations() {
    // `.llvm_stackmaps` uses absolute relocations for function addresses.
    if reloc.kind() != RelocationKind::Absolute {
      continue;
    }
    if reloc.size() != 64 {
      continue;
    }

    let sym_addr = match reloc.target() {
      RelocationTarget::Symbol(sym_idx) => file
        .symbol_by_index(sym_idx)
        .expect("relocation target symbol")
        .address(),
      // Section-relative relocations shouldn't appear in `.llvm_stackmaps` for our use, but ignore
      // them defensively.
      _ => continue,
    };

    let addend = reloc.addend();
    let value = (sym_addr as i64)
      .checked_add(addend)
      .expect("stackmaps relocation overflow");
    let value = u64::try_from(value).expect("stackmaps relocation value negative");

    let off = usize::try_from(off).expect("relocation offset overflows usize");
    data[off..off + 8].copy_from_slice(&value.to_le_bytes());
  }

  data
}
#[test]
fn runtime_abi_declares_raw_symbols_and_no_may_gc_wrappers() {
  let context = Context::create();
  let cg = native_js::CodeGen::new(&context, "runtime_abi_test");
  let fns = cg.runtime_abi().declare_all();

  let ir = cg.module_ir();

  // Raw runtime ABI symbols must be declared with addrspace(0) pointer types to match
  // `runtime-native`'s exported C ABI (important for LTO).
  assert!(ir.contains("declare ptr @rt_alloc"), "missing rt_alloc:\n{ir}");
  assert!(
    ir.contains("declare ptr @rt_alloc_pinned"),
    "missing rt_alloc_pinned:\n{ir}"
  );
  // ABI regression guards: `rt_alloc` / `rt_alloc_pinned` take `RtShapeId` as a 32-bit integer.
  let alloc_params = fns.rt_alloc.get_type().get_param_types();
  assert_eq!(alloc_params.len(), 2);
  assert_eq!(alloc_params[0].into_int_type().get_bit_width(), 64);
  assert_eq!(alloc_params[1].into_int_type().get_bit_width(), 32);

  let alloc_pinned_params = fns.rt_alloc_pinned.get_type().get_param_types();
  assert_eq!(alloc_pinned_params.len(), 2);
  assert_eq!(alloc_pinned_params[0].into_int_type().get_bit_width(), 64);
  assert_eq!(alloc_pinned_params[1].into_int_type().get_bit_width(), 32);

  // These are MayGC runtime functions and must not have `_gc` wrapper functions.
  assert!(
    !ir.contains("rt_alloc_gc"),
    "unexpected rt_alloc_gc wrapper function in IR:\n{ir}"
  );
  assert!(
    !ir.contains("rt_alloc_pinned_gc"),
    "unexpected rt_alloc_pinned_gc wrapper function in IR:\n{ir}"
  );
  assert!(
    !ir.contains("rt_gc_safepoint_gc"),
    "unexpected rt_gc_safepoint_gc wrapper function in IR:\n{ir}"
  );
  assert!(
    !ir.contains("rt_gc_collect_gc"),
    "unexpected rt_gc_collect_gc wrapper function in IR:\n{ir}"
  );

  assert!(
    ir.contains("declare ptr @rt_gc_safepoint_relocate_h"),
    "missing rt_gc_safepoint_relocate_h:\n{ir}"
  );
  assert!(
    !ir.contains("rt_gc_safepoint_relocate_h_gc"),
    "unexpected rt_gc_safepoint_relocate_h_gc wrapper function in IR:\n{ir}"
  );

  // NoGC calls with GC pointer parameters use leaf wrappers (`*_gc`).
  assert!(
    ir.contains("define internal void @rt_write_barrier_gc"),
    "missing rt_write_barrier_gc wrapper:\n{ir}"
  );
  let wb = function_block(&ir, "@rt_write_barrier_gc");
  assert!(
    wb.contains("store ptr @rt_write_barrier"),
    "expected rt_write_barrier_gc to indirect-call @rt_write_barrier:\n{wb}"
  );
  assert!(
    !wb.contains("addrspacecast"),
    "rt_write_barrier_gc must not addrspacecast GC pointers out of addrspace(1):\n{wb}"
  );
  assert!(
    wb.contains("notail call void %") && wb.contains("ptr addrspace(1)"),
    "expected rt_write_barrier_gc to emit a notail indirect call (prevent TCO):\n{wb}"
  );
  assert!(
    has_gc_leaf_attr(fns.rt_write_barrier_gc),
    "expected rt_write_barrier_gc to be marked as a gc-leaf-function:\n{ir}"
  );

  assert!(
    ir.contains("define internal void @rt_write_barrier_range_gc"),
    "missing rt_write_barrier_range_gc wrapper:\n{ir}"
  );
  let wbr = function_block(&ir, "@rt_write_barrier_range_gc");
  assert!(
    wbr.contains("store ptr @rt_write_barrier_range"),
    "expected rt_write_barrier_range_gc to indirect-call @rt_write_barrier_range:\n{wbr}"
  );
  assert!(
    !wbr.contains("addrspacecast"),
    "rt_write_barrier_range_gc must not addrspacecast GC pointers out of addrspace(1):\n{wbr}"
  );
  assert!(
    wbr.contains("notail call void %") && wbr.contains("ptr addrspace(1)"),
    "expected rt_write_barrier_range_gc to emit a notail indirect call (prevent TCO):\n{wbr}"
  );
  assert!(
    has_gc_leaf_attr(fns.rt_write_barrier_range_gc),
    "expected rt_write_barrier_range_gc to be marked as a gc-leaf-function:\n{ir}"
  );

  assert!(
    ir.contains("define internal void @rt_keep_alive_gc_ref_gc"),
    "missing rt_keep_alive_gc_ref_gc wrapper:\n{ir}"
  );
  let keep_alive_line = ir
    .lines()
    .find(|l| l.contains("define internal void @rt_keep_alive_gc_ref_gc"))
    .expect("rt_keep_alive_gc_ref_gc line");
  assert!(
    !keep_alive_line.contains("gc \"coreclr\""),
    "rt_keep_alive_gc_ref_gc must not be GC-managed (ABI wrappers are outside GC pointer discipline lint):\n{keep_alive_line}\n\nIR:\n{ir}"
  );
  let keep_alive = function_block(&ir, "@rt_keep_alive_gc_ref_gc");
  assert!(
    keep_alive.contains("store ptr @rt_keep_alive_gc_ref"),
    "expected rt_keep_alive_gc_ref_gc to indirect-call @rt_keep_alive_gc_ref:\n{keep_alive}"
  );
  assert!(
    !keep_alive.contains("addrspacecast"),
    "rt_keep_alive_gc_ref_gc must not addrspacecast GC pointers out of addrspace(1):\n{keep_alive}"
  );
  assert!(
    keep_alive.contains("notail call void %") && keep_alive.contains("ptr addrspace(1)"),
    "expected rt_keep_alive_gc_ref_gc to emit a notail indirect call (prevent TCO):\n{keep_alive}"
  );
  assert!(
    has_gc_leaf_attr(fns.rt_keep_alive_gc_ref_gc),
    "expected rt_keep_alive_gc_ref_gc to be marked as a gc-leaf-function:\n{ir}"
  );

  // Parallel scheduler entrypoints are raw ABI (no GC pointer wrapper needed).
  assert!(
    ir.contains("declare i64 @rt_parallel_spawn"),
    "missing rt_parallel_spawn declaration:\n{ir}"
  );
  assert!(
    ir.contains("declare void @rt_parallel_join"),
    "missing rt_parallel_join declaration:\n{ir}"
  );
  assert!(
    ir.contains("declare void @rt_parallel_for"),
    "missing rt_parallel_for declaration:\n{ir}"
  );
  // Scheduler entrypoints are MayGC (they may block/allocate). If we accidentally mark them as GC
  // leaf functions, LLVM will omit statepoint rewriting and we won't have stackmaps to enumerate
  // roots if a GC occurs while the mutator is inside the scheduler.
  assert!(
    !has_gc_leaf_attr(fns.rt_parallel_spawn),
    "rt_parallel_spawn must not be marked gc-leaf-function"
  );
  assert!(
    !has_gc_leaf_attr(fns.rt_parallel_join),
    "rt_parallel_join must not be marked gc-leaf-function"
  );
  assert!(
    !has_gc_leaf_attr(fns.rt_parallel_for),
    "rt_parallel_for must not be marked gc-leaf-function"
  );

  // ABI regression guards for scheduler signatures.
  let spawn_params = fns.rt_parallel_spawn.get_type().get_param_types();
  assert_eq!(spawn_params.len(), 2);
  assert!(spawn_params[0].is_pointer_type());
  assert!(spawn_params[1].is_pointer_type());

  let join_params = fns.rt_parallel_join.get_type().get_param_types();
  assert_eq!(join_params.len(), 2);
  assert!(join_params[0].is_pointer_type());
  assert_eq!(join_params[1].into_int_type().get_bit_width(), 64);

  let for_params = fns.rt_parallel_for.get_type().get_param_types();
  assert_eq!(for_params.len(), 4);
  assert_eq!(for_params[0].into_int_type().get_bit_width(), 64);
  assert_eq!(for_params[1].into_int_type().get_bit_width(), 64);
  assert!(for_params[2].is_pointer_type());
  assert!(for_params[3].is_pointer_type());
}

fn find_clang() -> Option<&'static str> {
  for candidate in ["clang", "clang-18"] {
    if Command::new(candidate)
      .arg("--version")
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .status()
      .is_ok()
    {
      return Some(candidate);
    }
  }
  None
}

#[test]
fn emitted_object_contains_llvm_stackmaps_section() {
  if !cfg!(unix) {
    eprintln!("skipping: stackmaps object emission test only runs on unix-like targets");
    return;
  }

  let Some(clang) = find_clang() else {
    eprintln!("skipping: no clang compiler available");
    return;
  };

  let context = Context::create();
  let module = context.create_module("stackmaps_test");
  let builder = context.create_builder();

  // Ensure runtime ABI declarations exist in the module (regression guard for the runtime ABI layer).
  RuntimeAbi::new(&context, &module).declare_all();

  // Force emission of the `.llvm_stackmaps` section via `llvm.experimental.stackmap`.
  let i32_ty = context.i32_type();
  let main_ty = i32_ty.fn_type(&[], false);
  let main = module.add_function("main", main_ty, None);
  let entry = context.append_basic_block(main, "entry");
  builder.position_at_end(entry);

  let stackmap_ty =
    context
      .void_type()
      .fn_type(&[context.i64_type().into(), context.i32_type().into()], true);
  let stackmap = module.add_function("llvm.experimental.stackmap", stackmap_ty, None);

  let _ = builder
    .build_call(
      stackmap,
      &[
        context.i64_type().const_int(0, false).into(),
        context.i32_type().const_int(0, false).into(),
      ],
      "",
    )
    .expect("build stackmap call");
  builder
    .build_return(Some(&i32_ty.const_int(0, false)))
    .expect("build ret");

  let ir = module.print_to_string().to_string();

  let tmp = tempfile::tempdir().expect("create tempdir");
  let ll_path = tmp.path().join("out.ll");
  let obj_path = tmp.path().join("out.o");
  std::fs::write(&ll_path, &ir).expect("write out.ll");

  let output = Command::new(clang)
    .arg("-x")
    .arg("ir")
    .arg("-c")
    .arg(&ll_path)
    .arg("-o")
    .arg(&obj_path)
    .output()
    .expect("run clang");

  assert!(
    output.status.success(),
    "clang failed:\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  let obj = std::fs::read(&obj_path).expect("read out.o");
  assert!(
    obj
      .windows(b".llvm_stackmaps".len())
      .any(|w| w == b".llvm_stackmaps"),
    "object missing .llvm_stackmaps section name; IR was:\n{ir}"
  );
}

#[test]
fn allocation_sites_use_raw_rt_alloc_symbol_from_gc_managed_code() {
  let context = Context::create();
  let module = context.create_module("runtime_alloc_indirect_call_test");
  let builder = context.create_builder();

  let abi = RuntimeAbi::new(&context, &module);

  let gc_ptr = context.ptr_type(AddressSpace::from(1u16));
  let fn_ty = gc_ptr.fn_type(&[], false);
  let func = module.add_function("alloc_site", fn_ty, None);
  gc::set_default_gc_strategy(&func).expect("GC strategy contains NUL byte");

  let entry = context.append_basic_block(func, "entry");
  builder.position_at_end(entry);

  let size = context.i64_type().const_int(16, false);
  let shape = context.i32_type().const_int(1, false);
  let call = abi
    .emit_runtime_call(
      &builder,
      RuntimeFn::Alloc,
      &[size.into(), shape.into()],
      "obj",
    )
    .expect("emit rt_alloc call");
  let obj = call
    .try_as_basic_value()
    .left()
    .expect("rt_alloc returns a value")
    .into_pointer_value();
  builder.build_return(Some(&obj)).expect("build ret");

  let ir = module.print_to_string().to_string();
  let func_ir = function_block(&ir, "@alloc_site");

  // Must refer to the raw runtime symbol, and must not call a `*_gc` wrapper (which would insert an
  // extra frame between managed code and the runtime).
  assert!(
    func_ir.contains("@rt_alloc") && !func_ir.contains("@rt_alloc_gc"),
    "expected alloc_site to use rt_alloc directly (no rt_alloc_gc wrapper):\n{func_ir}"
  );
  assert!(
    !func_ir.contains("addrspacecast"),
    "allocation call sequence must not hide GC pointers via addrspacecast:\n{func_ir}"
  );
}

#[test]
fn emitted_stackmaps_parse_and_verify_with_runtime_native() {
  let Some(arch) = runtime_native_arch() else {
    // runtime-native only supports x86_64/aarch64 stackmap verification today.
    return;
  };

  let context = Context::create();
  let module = context.create_module("runtime_abi_stackmaps_verify");
  let builder = context.create_builder();

  let abi = RuntimeAbi::new(&context, &module);

  let gc_ptr = gc::gc_ptr_type(&context);
  let fn_ty = gc_ptr.fn_type(&[], false);
  let func = module.add_function("alloc_site", fn_ty, None);
  gc::set_default_gc_strategy(&func).expect("GC strategy contains NUL byte");

  let entry = context.append_basic_block(func, "entry");
  builder.position_at_end(entry);

  let size = context.i64_type().const_int(16, false);
  let shape = context.i32_type().const_int(1, false);
  let call = abi
    .emit_runtime_call(
      &builder,
      RuntimeFn::Alloc,
      &[size.into(), shape.into()],
      "obj",
    )
    .expect("emit rt_alloc call");
  let obj = call
    .try_as_basic_value()
    .left()
    .expect("rt_alloc returns a value")
    .into_pointer_value();
  builder.build_return(Some(&obj)).expect("build ret");

  if let Err(err) = module.verify() {
    panic!("invalid module: {err}\n\nIR:\n{}", module.print_to_string());
  }

  let obj =
    emit_object_with_statepoints(&module, TargetConfig::default()).expect("emit object with statepoints");
  let file = object::File::parse(&*obj).expect("parse object file");
  let section = file
    .section_by_name(".llvm_stackmaps")
    .expect("missing .llvm_stackmaps section");
  let bytes = section.data().expect("read .llvm_stackmaps section");

  // `load_stackmap` runs the statepoint verifier in debug builds, asserting our "roots are spilled
  // to stack slots" invariants.
  let _ = load_stackmap(bytes, arch).expect("runtime-native stackmap verifier should accept output");
}

#[test]
fn no_missing_stackmap_gap_from_runtime_frame() {
  let Some(_arch) = runtime_native_arch() else {
    // runtime-native only supports x86_64/aarch64 stackmap parsing today.
    return;
  };

  let context = Context::create();
  let module = context.create_module("runtime_abi_stackwalk_gap");
  let builder = context.create_builder();

  let abi = RuntimeAbi::new(&context, &module);

  // Build a minimal GC-managed function with exactly one MayGC runtime call.
  let gc_ptr = gc::gc_ptr_type(&context);
  let fn_ty = gc_ptr.fn_type(&[], false);
  let func = module.add_function("alloc_site", fn_ty, None);
  gc::set_default_gc_strategy(&func).expect("GC strategy contains NUL byte");

  let entry = context.append_basic_block(func, "entry");
  builder.position_at_end(entry);

  let size = context.i64_type().const_int(16, false);
  let shape = context.i32_type().const_int(1, false);
  let call = abi
    .emit_runtime_call(
      &builder,
      RuntimeFn::Alloc,
      &[size.into(), shape.into()],
      "obj",
    )
    .expect("emit rt_alloc call");
  let obj = call
    .try_as_basic_value()
    .left()
    .expect("rt_alloc returns a value")
    .into_pointer_value();
  builder.build_return(Some(&obj)).expect("build ret");

  // Rewrite statepoints + emit object with `.llvm_stackmaps`.
  let obj =
    emit_object_with_statepoints(&module, TargetConfig::default()).expect("emit object with statepoints");

  let file = object::File::parse(&*obj).expect("parse object file");
  let stackmaps_section = file
    .section_by_name(".llvm_stackmaps")
    .expect("missing .llvm_stackmaps section");

  let relocated_stackmaps = relocate_stackmaps_section(&file, &stackmaps_section);
  let stackmaps =
    StackMaps::parse(&relocated_stackmaps).expect("parse stackmaps + build callsite index");

  assert!(
    !stackmaps.callsites().is_empty(),
    "expected at least one stackmap callsite, got 0.\nIR:\n{}",
    module.print_to_string()
  );

  let ir = module.print_to_string().to_string();
  let alloc_site_ir = function_block(&ir, "@alloc_site");

  // If we ever regress back to a wrapper-based ABI, the allocation safepoint would be for
  // `@rt_alloc_gc`, but GC running inside `@rt_alloc` would return into the wrapper frame, which
  // has no stackmap record => `MissingStackMap`.
  assert!(
    alloc_site_ir.contains("@llvm.experimental.gc.statepoint"),
    "expected alloc_site to contain a gc.statepoint after rewrite:\n\n{alloc_site_ir}"
  );
  if alloc_site_ir.contains("@rt_alloc_gc") {
    panic!(
      "alloc_site still calls rt_alloc_gc wrapper; this would create a MissingStackMap gap.\n\n{alloc_site_ir}"
    );
  }
  assert!(
    alloc_site_ir.contains("@rt_alloc"),
    "expected alloc_site to refer to rt_alloc:\n\n{alloc_site_ir}"
  );
}
