use inkwell::context::Context;
use inkwell::targets::{CodeModel, InitializationConfig, RelocMode, Target, TargetMachine};
use inkwell::OptimizationLevel;
use native_js::{emit, llvm::gc};
use object::Object;
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

  emit::write_object_file(&module, &tm, &obj).expect("failed to run statepoint rewrite + emit object file");

  let ir = module.print_to_string().to_string();
  assert!(
    ir.contains("gc \"coreclr\""),
    "expected `gc \"coreclr\"` on function\n{ir}"
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
}
