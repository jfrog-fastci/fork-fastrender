use inkwell::context::Context;
use inkwell::targets::{CodeModel, FileType, InitializationConfig, RelocMode, Target, TargetMachine};
use inkwell::OptimizationLevel;
use native_js::llvm::{gc, passes};
use std::process::Command;
use tempfile::tempdir;

#[test]
fn rewrite_statepoints_emits_stackmaps() {
  Target::initialize_native(&InitializationConfig::default()).expect("failed to init native target");

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
  gc::set_statepoint_example_gc(&test_fn).expect("GC strategy contains NUL byte");

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

  passes::rewrite_statepoints_for_gc(&module, &tm).expect("rewrite-statepoints-for-gc failed");

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

  let tmp = tempdir().expect("failed to create tempdir");
  let obj = tmp.path().join("statepoints.o");
  tm.write_to_file(&module, FileType::Object, &obj)
    .expect("failed to emit object file");

  let readobj = Command::new("llvm-readobj-18")
    .arg("--sections")
    .arg(&obj)
    .output()
    .expect("failed to run llvm-readobj-18");
  assert!(
    readobj.status.success(),
    "llvm-readobj-18 failed:\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&readobj.stdout),
    String::from_utf8_lossy(&readobj.stderr)
  );

  let stdout = String::from_utf8_lossy(&readobj.stdout);
  assert!(
    stdout.contains(".llvm_stackmaps"),
    "expected .llvm_stackmaps section in emitted object\nllvm-readobj-18 --sections output:\n{stdout}"
  );
}

