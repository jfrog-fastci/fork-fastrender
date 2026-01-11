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
    alloc.contains("call ptr addrspace(1) %"),
    "expected rt_alloc_gc to call a ptr addrspace(1) function pointer:\n{alloc}"
  );
  assert!(
    !alloc.contains("addrspacecast"),
    "rt_alloc_gc must not use addrspacecasts (would hide GC pointers):\n{alloc}"
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
    alloc_pinned.contains("call ptr addrspace(1) %"),
    "expected rt_alloc_pinned_gc to call a ptr addrspace(1) function pointer:\n{alloc_pinned}"
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
    wb.contains("call void %") && wb.contains("ptr addrspace(1)"),
    "expected rt_write_barrier_gc to call via function pointer with GC pointer args:\n{wb}"
  );
  assert!(
    !wb.contains("addrspacecast"),
    "rt_write_barrier_gc must not addrspacecast GC pointers out of addrspace(1):\n{wb}"
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
