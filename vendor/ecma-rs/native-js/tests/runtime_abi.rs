use inkwell::context::Context;
use native_js::runtime_abi::RuntimeAbi;
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
    keep_alive.contains("notail call void %") && keep_alive.contains("ptr addrspace(1)"),
    "expected rt_keep_alive_gc_ref_gc to emit a notail indirect call (prevent TCO):\n{keep_alive}"
  );
  assert!(
    !keep_alive.contains("addrspacecast"),
    "rt_keep_alive_gc_ref_gc must not addrspacecast GC pointers out of addrspace(1):\n{keep_alive}"
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

  let stackmap_ty = context.void_type().fn_type(
    &[context.i64_type().into(), context.i32_type().into()],
    true,
  );
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
