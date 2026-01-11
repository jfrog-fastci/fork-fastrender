use inkwell::context::Context;
use inkwell::targets::{CodeModel, RelocMode, Target, TargetMachine};
use inkwell::OptimizationLevel;
use native_js::{emit, llvm::gc};
use object::Object;
use std::process::{Command, Stdio};
use tempfile::tempdir;

#[test]
fn rewrite_statepoints_emits_stackmaps() {
  native_js::llvm::init_native_target().expect("failed to init native target");

  let context = Context::create();
  let module = context.create_module("statepoints");
  let builder = context.create_builder();

  let gc_ptr = gc::gc_ptr_type(&context);

  // declare void @callee()
  let callee_ty = context.void_type().fn_type(&[], false);
  let callee = module.add_function("callee", callee_ty, None);

  // define ptr addrspace(1) @test(ptr addrspace(1)) gc "statepoint-example"
  let test_ty = gc_ptr.fn_type(&[gc_ptr.into()], false);
  let test_fn = module.add_function("test", test_ty, None);
  gc::set_gc_strategy(&test_fn, "statepoint-example").expect("GC strategy contains NUL byte");

  let entry = context.append_basic_block(test_fn, "entry");
  builder.position_at_end(entry);

  // Ensure the GC pointer argument is live across the call.
  builder.build_call(callee, &[], "call_callee").unwrap();
  let arg0 = test_fn
    .get_first_param()
    .expect("missing arg0")
    .into_pointer_value();
  builder.build_return(Some(&arg0)).unwrap();

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

  module.set_triple(&triple);
  module.set_data_layout(&tm.get_target_data().get_data_layout());

  let tmp = tempdir().expect("failed to create tempdir");
  let obj = tmp.path().join("statepoints.o");

  emit::write_object_file(&module, &tm, &obj)
    .expect("failed to run statepoint rewrite + emit object file");

  let ir = module.print_to_string().to_string();
  assert!(
    ir.contains("gc \"statepoint-example\""),
    "expected `gc \"statepoint-example\"` on function\n{ir}"
  );
  assert!(
    ir.contains("ptr addrspace(1)"),
    "expected addrspace(1) pointers in IR\n{ir}"
  );
  assert!(
    ir.contains("llvm.experimental.gc.statepoint"),
    "expected statepoint intrinsic after rewriting\n{ir}"
  );
  assert!(
    ir.contains("gc.relocate"),
    "expected gc.relocate intrinsic after rewriting\n{ir}"
  );
  assert!(
    ir.contains("\"gc-live\""),
    "expected `\"gc-live\"` operand bundle after rewriting\n{ir}"
  );

  let bytes = std::fs::read(&obj).expect("read emitted object file");
  let file = object::File::parse(&*bytes).expect("parse emitted object file");
  assert!(
    file.section_by_name(".llvm_stackmaps").is_some(),
    "expected .llvm_stackmaps section in emitted object\nIR:\n{ir}"
  );

  // Cross-check via llvm-readobj (matches how we debug stackmap emission in
  // practice and ensures the external tool sees the section).
  let readobj = match Command::new("llvm-readobj-18")
    .arg("--sections")
    .arg(&obj)
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .output()
  {
    Ok(out) => out,
    Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
      eprintln!("skipping llvm-readobj-18 check: llvm-readobj-18 not found in PATH");
      return;
    }
    Err(err) => panic!("failed to run llvm-readobj-18: {err}"),
  };
  assert!(
    readobj.status.success(),
    "llvm-readobj-18 failed:\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&readobj.stdout),
    String::from_utf8_lossy(&readobj.stderr)
  );
  let stdout = String::from_utf8_lossy(&readobj.stdout);
  assert!(
    stdout.contains(".llvm_stackmaps"),
    "expected `.llvm_stackmaps` in llvm-readobj-18 output:\n{stdout}"
  );
}
