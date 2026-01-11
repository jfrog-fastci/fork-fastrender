use inkwell::context::Context;
use inkwell::targets::{CodeModel, RelocMode, Target, TargetMachine};
use inkwell::OptimizationLevel;
use native_js::llvm::{gc, passes};
use native_js::runtime_abi::{RuntimeAbi, RuntimeFn};

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
fn may_gc_runtime_calls_are_direct_and_become_statepoints() {
  native_js::llvm::init_native_target().expect("failed to init native target");

  let context = Context::create();
  let module = context.create_module("runtime_maygc_calls_are_direct");
  let builder = context.create_builder();

  // define ptr addrspace(1) @test() gc "coreclr" { ... }
  let gc_ptr = gc::gc_ptr_type(&context);
  let fn_ty = gc_ptr.fn_type(&[], false);
  let test_fn = module.add_function("test", fn_ty, None);
  gc::set_default_gc_strategy(&test_fn).expect("GC strategy contains NUL byte");

  let entry = context.append_basic_block(test_fn, "entry");
  builder.position_at_end(entry);

  let size = context.i64_type().const_int(16, false);
  let shape = context.i32_type().const_int(1, false);
  let rt = RuntimeAbi::new(&context, &module);
  let call = rt
    .emit_runtime_call(&builder, RuntimeFn::Alloc, &[size.into(), shape.into()], "obj")
    .expect("emit rt_alloc call");
  let obj = call
    .try_as_basic_value()
    .left()
    .expect("rt_alloc returns a value")
    .into_pointer_value();
  builder.build_return(Some(&obj)).expect("build ret");

  if let Err(err) = module.verify() {
    panic!("module verification failed: {err}\n\nIR:\n{}", module.print_to_string());
  }

  let ir = module.print_to_string().to_string();
  let func = function_block(&ir, "@test");
  assert!(
    func.contains("@rt_alloc"),
    "expected the allocating call to happen in @test (no wrapper frames):\n{func}\n\nFull IR:\n{ir}"
  );
  assert!(
    !ir.contains("rt_alloc_gc"),
    "must not call a native-js wrapper like rt_alloc_gc:\n{ir}"
  );

  // Now run `rewrite-statepoints-for-gc` and ensure the callsite was rewritten to a statepoint.
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

  if let Err(err) = module.verify() {
    panic!(
      "module verification failed after rewrite: {err}\n\nIR:\n{}",
      module.print_to_string()
    );
  }

  let rewritten = module.print_to_string().to_string();
  let func = function_block(&rewritten, "@test");
  assert!(
    func.contains("@llvm.experimental.gc.statepoint"),
    "expected statepoint after rewriting:\n{func}\n\nFull IR:\n{rewritten}"
  );
  assert!(
    func.contains("rt_alloc"),
    "expected rt_alloc to appear in the rewritten function:\n{func}\n\nFull IR:\n{rewritten}"
  );
  assert!(
    !rewritten.contains("rt_alloc_gc"),
    "must not reference rt_alloc_gc wrapper after rewrite:\n{rewritten}"
  );
}
