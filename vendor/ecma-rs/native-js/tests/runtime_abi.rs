use inkwell::context::Context;
use native_js::runtime_abi::RuntimeAbi;
use std::process::{Command, Stdio};

#[test]
fn runtime_wrappers_use_addrspacecasts() {
  let context = Context::create();
  let cg = native_js::CodeGen::new(&context, "runtime_abi_test");
  cg.runtime_abi().ensure_wrappers();

  let ir = cg.module_ir();

  assert!(
    ir.contains("define internal ptr addrspace(1) @rt_alloc_gc"),
    "missing rt_alloc_gc wrapper:\n{ir}"
  );
  assert!(
    ir.contains("addrspacecast ptr") && ir.contains("to ptr addrspace(1)"),
    "missing addrspacecast to addrspace(1) in rt_alloc_gc:\n{ir}"
  );

  assert!(
    ir.contains("define internal void @rt_write_barrier_gc"),
    "missing rt_write_barrier_gc wrapper:\n{ir}"
  );
  assert!(
    ir.contains("addrspacecast ptr addrspace(1)"),
    "missing addrspacecast from addrspace(1) in rt_write_barrier_gc:\n{ir}"
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

  // Ensure runtime ABI wrappers exist in the module (regression guard for the
  // addrspace wrapper strategy).
  RuntimeAbi::new(&context, &module).ensure_wrappers();

  // Force emission of the `.llvm_stackmaps` section via `llvm.experimental.stackmap`.
  let i32_ty = context.i32_type();
  let main_ty = i32_ty.fn_type(&[], false);
  let main = module.add_function("main", main_ty, None);
  let entry = context.append_basic_block(main, "entry");
  builder.position_at_end(entry);

  let stackmap_ty = context
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
