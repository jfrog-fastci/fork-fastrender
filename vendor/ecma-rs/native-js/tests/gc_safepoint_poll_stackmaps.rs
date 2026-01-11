use inkwell::context::Context;
use inkwell::targets::{CodeModel, RelocMode};
use inkwell::{IntPredicate, OptimizationLevel};
use native_js::{emit, llvm::gc};
use object::{Object as _, ObjectSection as _};
use runtime_native::stackmaps::StackMaps;

fn assigned_ssa(line: &str) -> Option<String> {
  let (lhs, _rhs) = line.split_once('=')?;
  Some(lhs.trim().to_string())
}

fn extract_ret_ssa(func_ir: &str) -> String {
  let ret_line = func_ir
    .lines()
    .find(|l| l.trim_start().starts_with("ret ptr addrspace(1)"))
    .unwrap_or_else(|| panic!("missing ret in function:\n{func_ir}"));

  ret_line
    .split_whitespace()
    .last()
    .unwrap_or("")
    .to_string()
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

/// Ensures `native-js` can insert cooperative GC polls even for call-free loops:
/// - LLVM `place-safepoints` must insert entry + backedge polls.
/// - `rewrite-statepoints-for-gc` must turn those polls into statepoints with relocations.
/// - Emitted `.llvm_stackmaps` must contain callsite records for the polls and pass the
///   runtime-native verifier.
#[test]
fn call_free_loop_has_safepoint_polls_with_stackmaps_and_relocation() {
  let context = Context::create();
  let module = context.create_module("gc_safepoint_poll_stackmaps");
  let builder = context.create_builder();

  let gc_ptr = gc::gc_ptr_type(&context);
  let i64_ty = context.i64_type();

  // define ptr addrspace(1) @test(ptr addrspace(1) %obj) gc "coreclr"
  //
  // The loop trip count is a compile-time constant so we exercise `place-safepoints`'s
  // `--spp-all-backedges` behavior (counted loops still get backedge polls).
  let fn_ty = gc_ptr.fn_type(&[gc_ptr.into()], false);
  let f = module.add_function("test", fn_ty, None);
  gc::set_default_gc_strategy(&f).expect("GC strategy contains NUL byte");

  let obj = f
    .get_nth_param(0)
    .expect("missing obj param")
    .into_pointer_value();

  let entry = context.append_basic_block(f, "entry");
  let loop_header = context.append_basic_block(f, "loop");
  let loop_body = context.append_basic_block(f, "loop.body");
  let exit = context.append_basic_block(f, "exit");

  builder.position_at_end(entry);
  builder
    .build_unconditional_branch(loop_header)
    .expect("br loop header");

  builder.position_at_end(loop_header);
  let i_phi = builder.build_phi(i64_ty, "i").expect("phi i");
  i_phi.add_incoming(&[(&i64_ty.const_zero(), entry)]);

  // Keep a GC pointer live across iterations (loop-carried) and across the inserted polls.
  let obj_phi = builder.build_phi(gc_ptr, "obj").expect("phi obj");
  obj_phi.add_incoming(&[(&obj, entry)]);

  let i = i_phi.as_basic_value().into_int_value();
  let n = i64_ty.const_int(1_000_000, false);
  let cond = builder
    .build_int_compare(IntPredicate::ULT, i, n, "cond")
    .expect("icmp");
  builder
    .build_conditional_branch(cond, loop_body, exit)
    .expect("brcond");

  builder.position_at_end(loop_body);
  let i_next = builder
    .build_int_add(i, i64_ty.const_int(1, false), "i.next")
    .expect("i.next");
  builder
    .build_unconditional_branch(loop_header)
    .expect("backedge");

  // Make `%obj` explicitly loop-carried.
  obj_phi.add_incoming(&[(&obj_phi.as_basic_value(), loop_body)]);
  i_phi.add_incoming(&[(&i_next, loop_body)]);

  builder.position_at_end(exit);
  let obj_out = obj_phi.as_basic_value().into_pointer_value();
  builder.build_return(Some(&obj_out)).expect("ret obj");

  if let Err(err) = module.verify() {
    panic!("input module verification failed: {err}\n\nIR:\n{}", module.print_to_string());
  }

  // Run codegen via the native-js helper which applies:
  //   place-safepoints + rewrite-statepoints-for-gc
  // and also configures the backend to spill statepoint roots to stack (required by runtime-native).
  let mut target = emit::TargetConfig::default();
  target.cpu = "generic".to_string();
  target.features = "".to_string();
  target.opt_level = OptimizationLevel::None;
  target.reloc_mode = RelocMode::Default;
  target.code_model = CodeModel::Default;

  let obj_bytes = emit::emit_object_with_statepoints(&module, target).expect("emit object with statepoints");

  // Validate the rewritten IR: poll calls must have become statepoints and the returned pointer
  // must be derived from a gc.relocate result.
  let ir = module.print_to_string().to_string();
  assert!(
    ir.contains("@llvm.experimental.gc.statepoint") && ir.contains("@gc.safepoint_poll"),
    "expected poll calls to be rewritten into statepoints:\n{ir}"
  );
  assert!(
    ir.contains("@llvm.experimental.gc.relocate.p1"),
    "expected gc.relocate for addrspace(1) pointers:\n{ir}"
  );

  let func_ir = function_block(&ir, "@test");
  let ret_ssa = extract_ret_ssa(&func_ir);
  let relocate_defs: std::collections::HashSet<String> = func_ir
    .lines()
    .filter(|l| l.contains("@llvm.experimental.gc.relocate.p1"))
    .filter_map(assigned_ssa)
    .collect();
  assert!(
    !relocate_defs.is_empty(),
    "expected at least one gc.relocate.p1 result:\n{func_ir}"
  );

  let mut phi_inputs: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
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
  let mut seen = std::collections::HashSet::new();
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

  // Parse `.llvm_stackmaps` and ensure:
  // - there are callsite records (entry + backedge poll),
  // - runtime-native's verifier accepts them,
  // - each callsite has at least one GC root slot.
  let file = object::File::parse(&*obj_bytes).expect("parse object file");
  let section = file
    .section_by_name(".llvm_stackmaps")
    .expect("missing .llvm_stackmaps section");
  let stackmaps_bytes = section.data().expect("read .llvm_stackmaps section");

  let stackmaps = StackMaps::parse(stackmaps_bytes).expect("parse stackmaps (includes verifier)");
  assert!(
    stackmaps.callsites().len() >= 2,
    "expected at least 2 stackmap callsites (entry + backedge poll), got {}",
    stackmaps.callsites().len()
  );

  for (pc, callsite) in stackmaps.iter() {
    let roots = callsite
      .gc_root_slots()
      .unwrap_or_else(|err| panic!("callsite 0x{pc:x} has invalid GC root locations: {err}"));
    assert!(
      !roots.is_empty(),
      "expected at least one GC root slot at callsite 0x{pc:x}"
    );
  }
}
