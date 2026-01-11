use inkwell::context::Context;
use native_js::runtime_abi::RuntimeAbi;
use std::process::{Command, Stdio};

#[test]
fn runtime_wrappers_do_not_addrspacecast_gc_pointers() {
  let context = Context::create();
  let cg = native_js::CodeGen::new(&context, "runtime_abi_test");
  cg.runtime_abi().ensure_wrappers();

  let ir = cg.module_ir();

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

  let rt_alloc_decl = ir
    .lines()
    .find(|l| l.starts_with("declare ptr @rt_alloc("))
    .expect("missing rt_alloc declaration");
  assert!(
    rt_alloc_decl.contains("i64") && rt_alloc_decl.contains("i32"),
    "rt_alloc declaration should take (i64, i32) params, got:\n{rt_alloc_decl}\n\nFull IR:\n{ir}"
  );

  let rt_alloc_gc_sig = ir
    .lines()
    .find(|l| l.starts_with("define internal ptr addrspace(1) @rt_alloc_gc("))
    .expect("missing rt_alloc_gc definition");
  assert!(
    rt_alloc_gc_sig.contains("i64") && rt_alloc_gc_sig.contains("i32"),
    "rt_alloc_gc wrapper should take (i64, i32) params, got:\n{rt_alloc_gc_sig}\n\nFull IR:\n{ir}"
  );

  assert!(
    ir.contains("define internal ptr addrspace(1) @rt_alloc_gc"),
    "missing rt_alloc_gc wrapper:\n{ir}"
  );
  let alloc_line = ir
    .lines()
    .find(|l| l.contains("define internal ptr addrspace(1) @rt_alloc_gc"))
    .expect("rt_alloc_gc line");
  assert!(
    !alloc_line.contains("gc \"coreclr\""),
    "rt_alloc_gc must not be GC-managed (ABI wrappers are outside GC pointer discipline lint):\n{alloc_line}\n\nIR:\n{ir}"
  );
  let alloc = function_block(&ir, "@rt_alloc_gc");
  assert!(
    alloc.contains("store ptr @rt_alloc"),
    "expected rt_alloc_gc to indirect-call @rt_alloc:\n{alloc}"
  );
  assert!(
    alloc.contains("notail call ptr addrspace(1) %"),
    "expected rt_alloc_gc to emit a notail indirect call (prevent TCO):\n{alloc}"
  );
  assert!(
    !alloc.contains("addrspacecast"),
    "rt_alloc_gc must not use addrspacecasts (would hide GC pointers):\n{alloc}"
  );

  let rt_alloc_pinned_decl = ir
    .lines()
    .find(|l| l.starts_with("declare ptr @rt_alloc_pinned("))
    .expect("missing rt_alloc_pinned declaration");
  assert!(
    rt_alloc_pinned_decl.contains("i64") && rt_alloc_pinned_decl.contains("i32"),
    "rt_alloc_pinned declaration should take (i64, i32) params, got:\n{rt_alloc_pinned_decl}\n\nFull IR:\n{ir}"
  );

  let rt_alloc_pinned_gc_sig = ir
    .lines()
    .find(|l| l.starts_with("define internal ptr addrspace(1) @rt_alloc_pinned_gc("))
    .expect("missing rt_alloc_pinned_gc definition");
  assert!(
    rt_alloc_pinned_gc_sig.contains("i64") && rt_alloc_pinned_gc_sig.contains("i32"),
    "rt_alloc_pinned_gc wrapper should take (i64, i32) params, got:\n{rt_alloc_pinned_gc_sig}\n\nFull IR:\n{ir}"
  );

  assert!(
    ir.contains("define internal ptr addrspace(1) @rt_alloc_pinned_gc"),
    "missing rt_alloc_pinned_gc wrapper:\n{ir}"
  );
  let alloc_pinned = function_block(&ir, "@rt_alloc_pinned_gc");
  assert!(
    alloc_pinned.contains("store ptr @rt_alloc_pinned"),
    "expected rt_alloc_pinned_gc to indirect-call @rt_alloc_pinned:\n{alloc_pinned}"
  );
  assert!(
    alloc_pinned.contains("notail call ptr addrspace(1) %"),
    "expected rt_alloc_pinned_gc to emit a notail indirect call (prevent TCO):\n{alloc_pinned}"
  );
  assert!(
    !alloc_pinned.contains("addrspacecast"),
    "rt_alloc_pinned_gc must not use addrspacecasts:\n{alloc_pinned}"
  );

  assert!(
    ir.contains("define internal void @rt_write_barrier_gc"),
    "missing rt_write_barrier_gc wrapper:\n{ir}"
  );
  let wb_line = ir
    .lines()
    .find(|l| l.contains("define internal void @rt_write_barrier_gc"))
    .expect("rt_write_barrier_gc line");
  assert!(
    !wb_line.contains("gc \"coreclr\""),
    "rt_write_barrier_gc must not be GC-managed (ABI wrappers are outside GC pointer discipline lint):\n{wb_line}\n\nIR:\n{ir}"
  );
  let wb = function_block(&ir, "@rt_write_barrier_gc");
  assert!(
    wb.contains("store ptr @rt_write_barrier"),
    "expected rt_write_barrier_gc to indirect-call @rt_write_barrier:\n{wb}"
  );
  assert!(
    wb.contains("notail call void %") && wb.contains("ptr addrspace(1)"),
    "expected rt_write_barrier_gc to emit a notail indirect call (prevent TCO):\n{wb}"
  );
  assert!(
    !wb.contains("addrspacecast"),
    "rt_write_barrier_gc must not addrspacecast GC pointers out of addrspace(1):\n{wb}"
  );

  assert!(
    ir.contains("define internal void @rt_gc_safepoint_gc"),
    "missing rt_gc_safepoint_gc wrapper:\n{ir}"
  );
  let safepoint_line = ir
    .lines()
    .find(|l| l.contains("define internal void @rt_gc_safepoint_gc"))
    .expect("rt_gc_safepoint_gc line");
  assert!(
    !safepoint_line.contains("gc \"coreclr\""),
    "rt_gc_safepoint_gc must not be GC-managed (ABI wrappers are outside GC pointer discipline lint):\n{safepoint_line}\n\nIR:\n{ir}"
  );
  let safepoint = function_block(&ir, "@rt_gc_safepoint_gc");
  assert!(
    safepoint.contains("@rt_gc_safepoint"),
    "expected rt_gc_safepoint_gc to call @rt_gc_safepoint:\n{safepoint}"
  );
  assert!(
    !safepoint.contains("addrspacecast"),
    "rt_gc_safepoint_gc must not use addrspacecasts:\n{safepoint}"
  );

  assert!(
    ir.contains("define internal void @rt_gc_collect_gc"),
    "missing rt_gc_collect_gc wrapper:\n{ir}"
  );
  let collect_line = ir
    .lines()
    .find(|l| l.contains("define internal void @rt_gc_collect_gc"))
    .expect("rt_gc_collect_gc line");
  assert!(
    !collect_line.contains("gc \"coreclr\""),
    "rt_gc_collect_gc must not be GC-managed (ABI wrappers are outside GC pointer discipline lint):\n{collect_line}\n\nIR:\n{ir}"
  );
  let collect = function_block(&ir, "@rt_gc_collect_gc");
  assert!(
    collect.contains("@rt_gc_collect"),
    "expected rt_gc_collect_gc to call @rt_gc_collect:\n{collect}"
  );
  assert!(
    !collect.contains("addrspacecast"),
    "rt_gc_collect_gc must not use addrspacecasts:\n{collect}"
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
    keep_alive.contains("call void %") && keep_alive.contains("ptr addrspace(1)"),
    "expected rt_keep_alive_gc_ref_gc to call via function pointer with GC pointer arg:\n{keep_alive}"
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

  // Ensure runtime ABI wrappers exist in the module (regression guard for the runtime ABI layer).
  RuntimeAbi::new(&context, &module).ensure_wrappers();

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
