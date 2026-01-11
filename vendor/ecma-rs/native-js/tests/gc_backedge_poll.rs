use inkwell::context::Context;
use inkwell::targets::{CodeModel, InitializationConfig, RelocMode, Target, TargetMachine};
use inkwell::IntPredicate;
use inkwell::OptimizationLevel;
use native_js::codegen::safepoint;
use native_js::llvm::{gc, passes};
use std::collections::{HashMap, HashSet};

fn block_body(ir: &str, block: &str) -> String {
  let label_prefix = format!("{block}:");
  let Some(start) = ir.lines().position(|l| l.starts_with(&label_prefix)) else {
    panic!("missing block {block} in IR:\n{ir}");
  };

  let mut end = ir.lines().count();
  for (i, line) in ir.lines().enumerate().skip(start + 1) {
    if !line.starts_with(' ') && !line.starts_with('\t') && line.contains(':') {
      end = i;
      break;
    }
  }

  ir.lines()
    .skip(start + 1)
    .take(end - (start + 1))
    .collect::<Vec<_>>()
    .join("\n")
}

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

fn assigned_ssa(line: &str) -> Option<String> {
  let (lhs, _rhs) = line.split_once('=')?;
  Some(lhs.trim().to_string())
}

fn extract_ret_ssa(func_ir: &str) -> String {
  let ret_line = func_ir
    .lines()
    .find(|l| l.trim_start().starts_with("ret ptr addrspace(1)"))
    .unwrap_or_else(|| panic!("missing ret in function:\n{func_ir}"));

  // `ret ptr addrspace(1) %x`
  ret_line
    .split_whitespace()
    .last()
    .unwrap_or("")
    .to_string()
}

#[test]
fn inserts_backedge_poll_and_rewrites_safepoint_to_statepoint() {
  Target::initialize_native(&InitializationConfig::default()).expect("failed to init native target");

  let context = Context::create();
  let module = context.create_module("gc_backedge_poll_test");
  let builder = context.create_builder();

  let gc_ptr = gc::gc_ptr_type(&context);
  let i64_ty = context.i64_type();

  // Define: ptr addrspace(1) @loop(ptr addrspace(1) %obj, i64 %n) gc "coreclr"
  let fn_ty = gc_ptr.fn_type(&[gc_ptr.into(), i64_ty.into()], false);
  let func = module.add_function("loop", fn_ty, None);
  gc::set_default_gc_strategy(&func).expect("GC strategy contains NUL byte");

  let entry = context.append_basic_block(func, "entry");
  let loop_header = context.append_basic_block(func, "loop.header");
  let loop_body = context.append_basic_block(func, "loop.body");
  let loop_latch = context.append_basic_block(func, "loop.latch");
  let exit = context.append_basic_block(func, "exit");

  builder.position_at_end(entry);
  let i_slot = builder.build_alloca(i64_ty, "i").expect("alloca i");
  builder
    .build_store(i_slot, i64_ty.const_zero())
    .expect("store i=0");
  builder
    .build_unconditional_branch(loop_header)
    .expect("br loop.header");

  builder.position_at_end(loop_header);
  let i_val = builder
    .build_load(i64_ty, i_slot, "i.val")
    .expect("load i")
    .into_int_value();
  let n = func
    .get_nth_param(1)
    .expect("missing n")
    .into_int_value();
  let cond = builder
    .build_int_compare(IntPredicate::ULT, i_val, n, "cond")
    .expect("icmp");
  builder
    .build_conditional_branch(cond, loop_body, exit)
    .expect("brcond");

  builder.position_at_end(loop_body);
  let i_next = builder
    .build_int_add(i_val, i64_ty.const_int(1, false), "i.next")
    .expect("add");
  builder.build_store(i_slot, i_next).expect("store i.next");
  builder
    .build_unconditional_branch(loop_latch)
    .expect("br loop.latch");

  builder.position_at_end(loop_latch);
  // This is the key: a tight loop with no calls except our explicit GC poll.
  safepoint::emit_backedge_gc_poll(&context, &module, &builder, func);
  // Poll helper positions the builder at `gc.poll.cont`.
  builder
    .build_unconditional_branch(loop_header)
    .expect("backedge br");

  builder.position_at_end(exit);
  let obj = func
    .get_nth_param(0)
    .expect("missing obj")
    .into_pointer_value();
  builder.build_return(Some(&obj)).expect("ret obj");

  // Run the statepoint rewrite pass so the slow-path call becomes a statepoint
  // with stack maps + gc.relocate.
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
    panic!("module verification failed: {err}\n\nIR:\n{}", module.print_to_string());
  }

  let ir = module.print_to_string().to_string();

  // Fast-path poll should inline the exported `RT_GC_EPOCH` flag in the backedge
  // block (no call/statepoint on the fast path).
  let latch_ir = block_body(&ir, "loop.latch");
  assert!(latch_ir.contains("@RT_GC_EPOCH"), "IR missing epoch load:\n{ir}");

  // Slow-path call should be rewritten into a statepoint.
  let func_ir = function_block(&ir, "@loop");
  let slow_ir = block_body(&ir, "gc.poll.slow");
  assert!(
    slow_ir.contains("@llvm.experimental.gc.statepoint"),
    "IR missing statepoint in slow path:\n{ir}"
  );
  assert!(
    slow_ir.contains("rt_gc_safepoint_slow"),
    "expected slow-path safepoint call to target rt_gc_safepoint_slow:\n{slow_ir}"
  );

  assert!(
    func_ir.contains("@llvm.experimental.gc.relocate.p1"),
    "expected gc.relocate.p1 in rewritten loop function:\n{func_ir}"
  );

  // Ensure the returned GC pointer is (directly or indirectly) derived from a relocated value.
  let ret_ssa = extract_ret_ssa(&func_ir);
  let relocate_defs: HashSet<String> = func_ir
    .lines()
    .filter(|l| l.contains("@llvm.experimental.gc.relocate.p1"))
    .filter_map(assigned_ssa)
    .collect();
  assert!(
    !relocate_defs.is_empty(),
    "expected at least one gc.relocate.p1 result:\n{func_ir}"
  );

  // Build a simple graph of SSA -> incoming SSA values for phi nodes, then walk
  // backwards from the returned value until we find a gc.relocate result.
  let mut phi_inputs: HashMap<String, Vec<String>> = HashMap::new();
  for line in func_ir.lines() {
    if !line.contains("= phi ") {
      continue;
    }
    let Some(lhs) = assigned_ssa(line) else {
      continue;
    };

    let mut inputs = Vec::new();
    for part in line.split('[').skip(1) {
      let Some(val) = part.split(',').next() else { continue };
      let val = val.trim();
      if !val.is_empty() {
        inputs.push(val.to_string());
      }
    }

    if !inputs.is_empty() {
      phi_inputs.insert(lhs, inputs);
    }
  }

  let mut stack = vec![ret_ssa.clone()];
  let mut seen = HashSet::new();
  let mut found_relocate = false;
  while let Some(v) = stack.pop() {
    if relocate_defs.contains(&v) {
      found_relocate = true;
      break;
    }
    if !seen.insert(v.clone()) {
      continue;
    }
    if let Some(inputs) = phi_inputs.get(&v) {
      for inp in inputs {
        stack.push(inp.clone());
      }
    }
  }

  assert!(
    found_relocate,
    "expected returned GC pointer ({ret_ssa}) to depend on a gc.relocate result, but could not find one.\n\n{func_ir}"
  );
}
