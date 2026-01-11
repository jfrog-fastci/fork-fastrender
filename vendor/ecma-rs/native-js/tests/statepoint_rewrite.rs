use inkwell::context::Context;
use inkwell::memory_buffer::MemoryBuffer;
use inkwell::targets::{CodeModel, FileType, RelocMode, Target, TargetMachine};
use inkwell::OptimizationLevel;
use native_js::llvm::passes;
use object::Object;
use std::fs;
use tempfile::tempdir;

fn init_llvm() {
  native_js::llvm::init_native_target().expect("failed to initialize native LLVM target");
}

fn host_target_machine() -> TargetMachine {
  init_llvm();

  let triple = TargetMachine::get_default_triple();
  let target = Target::from_triple(&triple).expect("host target");
  let cpu = TargetMachine::get_host_cpu_name().to_string();
  let features = TargetMachine::get_host_cpu_features().to_string();

  target
    .create_target_machine(
      &triple,
      &cpu,
      &features,
      OptimizationLevel::None,
      RelocMode::Default,
      CodeModel::Default,
    )
    .expect("create target machine")
}

fn with_rewritten_module<R>(
  llvm_ir: &str,
  f: impl for<'ctx> FnOnce(&inkwell::module::Module<'ctx>, &TargetMachine) -> R,
) -> R {
  init_llvm();

  let context = Context::create();
  let buffer = MemoryBuffer::create_from_memory_range_copy(llvm_ir.as_bytes(), "test.ll");
  let module = context
    .create_module_from_ir(buffer)
    .unwrap_or_else(|err| panic!("failed to parse LLVM IR: {err}\n\nIR:\n{llvm_ir}"));

  let tm = host_target_machine();
  module.set_triple(&tm.get_triple());
  module.set_data_layout(&tm.get_target_data().get_data_layout());

  passes::rewrite_statepoints_for_gc(&module, &tm).unwrap_or_else(|err| {
    panic!(
      "failed to run rewrite-statepoints-for-gc: {err}\n\nBefore:\n{llvm_ir}\n\nAfter:\n{}",
      module.print_to_string()
    )
  });

  if let Err(err) = module.verify() {
    panic!(
      "module verification failed after rewrite-statepoints-for-gc: {err}\n\nIR:\n{}",
      module.print_to_string()
    );
  }

  f(&module, &tm)
}

fn rewritten_ir(llvm_ir: &str) -> String {
  with_rewritten_module(llvm_ir, |m, _tm| m.print_to_string().to_string())
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
  let lhs = lhs.trim();
  lhs.strip_prefix('%').map(|_| lhs.to_string())
}

fn extract_first_arg_after(needle: &str, line: &str) -> Option<String> {
  let idx = line.find(needle)?;
  let rest = &line[idx + needle.len()..];
  let token = rest
    .trim_start()
    .split_whitespace()
    .next()
    .unwrap_or("")
    .trim_end_matches(|c| c == ',' || c == ')');
  if token.is_empty() {
    None
  } else {
    Some(token.to_string())
  }
}

fn parse_relocate_indices(line: &str) -> Option<(u32, u32)> {
  if !line.contains("@llvm.experimental.gc.relocate.p1") {
    return None;
  }

  let mut parts = line.split(',');
  let _tok = parts.next()?;
  let base_part = parts.next()?.trim();
  let derived_part = parts.next()?.trim();

  fn parse_i32_constant(s: &str) -> Option<u32> {
    let mut it = s.split_whitespace();
    let _ty = it.next()?;
    let value = it.next()?.trim_end_matches(')');
    value.parse().ok()
  }

  Some((
    parse_i32_constant(base_part)?,
    parse_i32_constant(derived_part)?,
  ))
}

fn parse_gc_live_vars(statepoint_line: &str) -> Vec<String> {
  fn extract_paren_contents(line: &str, marker: &str) -> String {
    let start = line
      .find(marker)
      .unwrap_or_else(|| panic!("missing `{marker}` in line: {line}"));
    let mut idx = start + marker.len();

    // `marker` includes the opening `(`.
    let mut depth: i32 = 1;
    let bytes = line.as_bytes();
    while idx < bytes.len() {
      match bytes[idx] {
        b'(' => depth += 1,
        b')' => {
          depth -= 1;
          if depth == 0 {
            let inner = &line[(start + marker.len())..idx];
            return inner.to_string();
          }
        }
        _ => {}
      }
      idx += 1;
    }

    panic!("unterminated `{marker}` (...) in line: {line}");
  }

  let inside = extract_paren_contents(statepoint_line, "\"gc-live\"(");

  inside
    .split(',')
    .map(str::trim)
    .filter(|s| !s.is_empty())
    .map(|s| {
      s.split_whitespace()
        .last()
        .unwrap_or_else(|| panic!("malformed gc-live entry `{s}` in line: {statepoint_line}"))
        .to_string()
    })
    .collect()
}

fn parse_relocate_comment_vars(line: &str) -> Option<(String, String)> {
  let (_, after) = line.split_once("; (")?;
  let (inside, _) = after.split_once(')')?;
  let (base, derived) = inside.split_once(',')?;
  Some((base.trim().to_string(), derived.trim().to_string()))
}

fn expect_relocate_line<'a>(func: &'a str, base: &str, derived: &str) -> &'a str {
  func
    .lines()
    .find(|line| parse_relocate_comment_vars(line) == Some((base.to_string(), derived.to_string())))
    .unwrap_or_else(|| {
      panic!(
        "expected gc.relocate line with comment `; ({base}, {derived})`, but none found.\n\n{func}"
      )
    })
}

#[test]
fn gc_result_for_scalar_return() {
  let before = r#"
  declare i64 @bar()

define i64 @test() gc "coreclr" {
entry:
  %x = call i64 @bar()
  ret i64 %x
}
"#;

  let after = rewritten_ir(before);

  assert!(
    after.contains("@llvm.experimental.gc.result.i64"),
    "missing gc.result.i64 intrinsic:\n{after}"
  );

  let func = function_block(&after, "@test");
  let statepoint_line = func
    .lines()
    .find(|l| l.contains("@llvm.experimental.gc.statepoint"))
    .unwrap_or_else(|| panic!("missing gc.statepoint call in function:\n{func}"));
  assert!(
    statepoint_line.contains("call token"),
    "expected gc.statepoint callsite to be a `call token`, got:\n{statepoint_line}\n\n{func}"
  );
  let statepoint_token = assigned_ssa(statepoint_line)
    .unwrap_or_else(|| panic!("unexpected statepoint line: {statepoint_line}"));

  assert!(
    !func.contains("call i64 @bar"),
    "expected @bar call to be rewritten (no direct call i64 @bar):\n{func}"
  );

  let gc_result_line = func
    .lines()
    .find(|l| l.contains("@llvm.experimental.gc.result.i64"))
    .unwrap_or_else(|| panic!("missing gc.result in function:\n{func}"));
  assert!(
    gc_result_line.contains(&format!("token {statepoint_token}")),
    "expected gc.result to reference statepoint token {statepoint_token}, got:\n{gc_result_line}\n\n{func}"
  );
  let result_ssa = assigned_ssa(gc_result_line)
    .unwrap_or_else(|| panic!("unexpected gc.result line: {gc_result_line}"));

  assert!(
    func.contains(&format!("ret i64 {result_ssa}")),
    "expected function to return gc.result value ({result_ssa}):\n{func}"
  );
}

#[test]
fn gc_result_for_gc_pointer_return() {
  let before = r#"
 declare ptr addrspace(1) @alloc()

define ptr addrspace(1) @test() gc "coreclr" {
entry:
  %p = call ptr addrspace(1) @alloc()
  ret ptr addrspace(1) %p
}
"#;

  let after = rewritten_ir(before);

  assert!(
    after.contains("@llvm.experimental.gc.result.p1"),
    "missing gc.result.p1 intrinsic:\n{after}"
  );

  let func = function_block(&after, "@test");
  let statepoint_line = func
    .lines()
    .find(|l| l.contains("@llvm.experimental.gc.statepoint"))
    .unwrap_or_else(|| panic!("missing gc.statepoint call in function:\n{func}"));
  assert!(
    statepoint_line.contains("call token"),
    "expected gc.statepoint callsite to be a `call token`, got:\n{statepoint_line}\n\n{func}"
  );
  let statepoint_token = assigned_ssa(statepoint_line)
    .unwrap_or_else(|| panic!("unexpected statepoint line: {statepoint_line}"));

  assert!(
    !func.contains("call ptr addrspace(1) @alloc"),
    "expected @alloc call to be rewritten (no direct call ptr addrspace(1) @alloc):\n{func}"
  );

  assert!(
    func.contains("ret ptr addrspace(1) %"),
    "expected return type to remain ptr addrspace(1):\n{func}"
  );

  let gc_result_line = func
    .lines()
    .find(|l| l.contains("@llvm.experimental.gc.result.p1"))
    .unwrap_or_else(|| panic!("missing gc.result.p1 in function:\n{func}"));
  assert!(
    gc_result_line.contains(&format!("token {statepoint_token}")),
    "expected gc.result to reference statepoint token {statepoint_token}, got:\n{gc_result_line}\n\n{func}"
  );
  let result_ssa = assigned_ssa(gc_result_line)
    .unwrap_or_else(|| panic!("unexpected gc.result line: {gc_result_line}"));

  assert!(
    func.contains(&format!("ret ptr addrspace(1) {result_ssa}")),
    "expected function to return gc.result value ({result_ssa}):\n{func}"
  );
}

#[test]
fn gc_result_for_scalar_return_with_live_gc_pointer() {
  // `gc.result` extraction must still work when the statepoint also carries a
  // `"gc-live"` bundle and therefore emits relocations.
  let before = r#"
  declare i64 @bar()

  define i64 @test(ptr addrspace(1) %obj) gc "coreclr" {
  entry:
    %v = call i64 @bar()
    %isnull = icmp eq ptr addrspace(1) %obj, null
    ret i64 %v
  }
  "#;

  let after = rewritten_ir(before);
  let func = function_block(&after, "@test");

  let statepoint_line = func
    .lines()
    .find(|l| l.contains("elementtype(i64 ()) @bar"))
    .unwrap_or_else(|| panic!("missing @bar statepoint call in function:\n{func}"));
  let statepoint_token = assigned_ssa(statepoint_line)
    .unwrap_or_else(|| panic!("unexpected statepoint line (not an assignment): {statepoint_line}"));

  let gc_live_vars = parse_gc_live_vars(statepoint_line);
  assert_eq!(
    gc_live_vars,
    vec!["%obj".to_string()],
    "expected @bar statepoint gc-live to be exactly [%obj], got {gc_live_vars:?}\n\n{func}"
  );

  let gc_result_line = func
    .lines()
    .find(|l| l.contains("@llvm.experimental.gc.result.i64"))
    .unwrap_or_else(|| panic!("missing gc.result.i64 in function:\n{func}"));
  assert!(
    gc_result_line.contains(&format!("token {statepoint_token}")),
    "expected gc.result to reference statepoint token {statepoint_token}, got:\n{gc_result_line}\n\n{func}"
  );
  let result_ssa = assigned_ssa(gc_result_line)
    .unwrap_or_else(|| panic!("unexpected gc.result line: {gc_result_line}"));

  let reloc_line = expect_relocate_line(&func, "%obj", "%obj");
  let relocated_obj = assigned_ssa(reloc_line)
    .unwrap_or_else(|| panic!("expected relocate assignment: {reloc_line}"));
  assert!(
    reloc_line.contains(&format!("token {statepoint_token}")),
    "expected gc.relocate to reference statepoint token {statepoint_token}, got:\n{reloc_line}\n\n{func}"
  );

  let (base_idx, derived_idx) = parse_relocate_indices(reloc_line)
    .unwrap_or_else(|| panic!("failed to parse relocate indices: {reloc_line}"));
  assert_eq!(
    (base_idx, derived_idx),
    (0, 0),
    "expected base/derived indices (0,0) for %obj relocation, got ({base_idx},{derived_idx})\n\n{func}"
  );

  assert!(
    func.contains(&format!("icmp eq ptr addrspace(1) {relocated_obj}, null")),
    "expected %obj use after safepoint to be rewritten to relocated value {relocated_obj}:\n{func}"
  );
  assert!(
    func.contains(&format!("ret i64 {result_ssa}")),
    "expected function to return gc.result value {result_ssa}:\n{func}"
  );
}

#[test]
fn gc_result_for_gc_pointer_return_with_live_gc_pointer() {
  // `gc.result.p1` must still work when the statepoint also carries `"gc-live"`
  // roots and emits relocations.
  let before = r#"
  declare ptr addrspace(1) @alloc()

  define ptr addrspace(1) @test(ptr addrspace(1) %obj) gc "coreclr" {
  entry:
    %p = call ptr addrspace(1) @alloc()
    %isnull = icmp eq ptr addrspace(1) %obj, null
    ret ptr addrspace(1) %p
  }
  "#;

  let after = rewritten_ir(before);
  let func = function_block(&after, "@test");

  assert!(
    !func.contains("call ptr addrspace(1) @alloc"),
    "expected @alloc call to be rewritten (no direct call ptr addrspace(1) @alloc):\n{func}"
  );

  let statepoint_line = func
    .lines()
    .find(|l| l.contains("elementtype(ptr addrspace(1) ()) @alloc"))
    .unwrap_or_else(|| panic!("missing @alloc statepoint call in function:\n{func}"));
  let statepoint_token = assigned_ssa(statepoint_line)
    .unwrap_or_else(|| panic!("unexpected statepoint line (not an assignment): {statepoint_line}"));

  let gc_live_vars = parse_gc_live_vars(statepoint_line);
  assert_eq!(
    gc_live_vars,
    vec!["%obj".to_string()],
    "expected @alloc statepoint gc-live to be exactly [%obj], got {gc_live_vars:?}\n\n{func}"
  );

  let gc_result_line = func
    .lines()
    .find(|l| l.contains("@llvm.experimental.gc.result.p1"))
    .unwrap_or_else(|| panic!("missing gc.result.p1 in function:\n{func}"));
  assert!(
    gc_result_line.contains(&format!("token {statepoint_token}")),
    "expected gc.result to reference statepoint token {statepoint_token}, got:\n{gc_result_line}\n\n{func}"
  );
  let result_ssa = assigned_ssa(gc_result_line)
    .unwrap_or_else(|| panic!("unexpected gc.result line: {gc_result_line}"));

  let reloc_line = expect_relocate_line(&func, "%obj", "%obj");
  let relocated_obj = assigned_ssa(reloc_line)
    .unwrap_or_else(|| panic!("expected relocate assignment: {reloc_line}"));
  assert!(
    func.contains(&format!("icmp eq ptr addrspace(1) {relocated_obj}, null")),
    "expected %obj use after safepoint to be rewritten to relocated value {relocated_obj}:\n{func}"
  );

  assert!(
    func.contains(&format!("ret ptr addrspace(1) {result_ssa}")),
    "expected function to return addrspace(1) gc.result value {result_ssa}:\n{func}"
  );
}

#[test]
fn gc_pointer_call_argument_is_in_gc_live_even_if_dead_after_call() {
  // A GC pointer that is *only* passed as an argument to a safepoint call (and
  // not used afterwards) still must be included in the `"gc-live"` list so the
  // GC can find/relocate it during the call.
  let before = r#"
  declare void @callee(ptr addrspace(1))

  define void @test(ptr addrspace(1) %obj) gc "coreclr" {
  entry:
    call void @callee(ptr addrspace(1) %obj)
    ret void
  }
  "#;

  let after = rewritten_ir(before);
  let func = function_block(&after, "@test");

  let statepoint_line = func
    .lines()
    .find(|l| l.contains("elementtype(void (ptr addrspace(1))) @callee"))
    .unwrap_or_else(|| panic!("missing @callee statepoint call in function:\n{func}"));
  assert!(
    statepoint_line.contains("i32 1, i32 0, ptr addrspace(1) %obj"),
    "expected statepoint call to include 1 call arg `%obj`, got:\n{statepoint_line}\n\n{func}"
  );

  let statepoint_token = assigned_ssa(statepoint_line)
    .unwrap_or_else(|| panic!("unexpected statepoint line (not an assignment): {statepoint_line}"));

  let gc_live_vars = parse_gc_live_vars(statepoint_line);
  assert_eq!(
    gc_live_vars,
    vec!["%obj".to_string()],
    "expected gc-live to contain only %obj, got {gc_live_vars:?}\n\n{func}"
  );

  let reloc_line = expect_relocate_line(&func, "%obj", "%obj");
  assert!(
    reloc_line.contains(&format!("token {statepoint_token}")),
    "expected gc.relocate to reference statepoint token {statepoint_token}, got:\n{reloc_line}\n\n{func}"
  );

  let (base_idx, derived_idx) =
    parse_relocate_indices(reloc_line).unwrap_or_else(|| panic!("failed to parse relocate indices: {reloc_line}"));
  assert_eq!(
    (base_idx, derived_idx),
    (0, 0),
    "expected base/derived indices (0,0) for %obj relocation, got ({base_idx},{derived_idx})\n\n{func}"
  );
}

#[test]
fn gc_pointer_select_is_relocated_across_safepoint() {
  // JS runtimes frequently have control-flow-dependent pointer selection (e.g.
  // polymorphic inline caches). Ensure a `select`-produced GC pointer is tracked
  // and rewritten across a safepoint.
  let before = r#"
  declare void @bar()

  define ptr addrspace(1) @test(i1 %cond, ptr addrspace(1) %a, ptr addrspace(1) %b) gc "coreclr" {
  entry:
    %p = select i1 %cond, ptr addrspace(1) %a, ptr addrspace(1) %b
    call void @bar()
    ret ptr addrspace(1) %p
  }
  "#;

  let after = rewritten_ir(before);
  let func = function_block(&after, "@test");

  let statepoint_line = func
    .lines()
    .find(|l| l.contains("elementtype(void ()) @bar"))
    .unwrap_or_else(|| panic!("missing @bar statepoint call in function:\n{func}"));
  let gc_live = parse_gc_live_vars(statepoint_line);
  assert_eq!(
    gc_live,
    vec!["%p".to_string()],
    "expected statepoint gc-live to contain only %p, got {gc_live:?}\n\n{func}"
  );

  let reloc_line = expect_relocate_line(&func, "%p", "%p");
  let relocated_p =
    assigned_ssa(reloc_line).unwrap_or_else(|| panic!("expected relocate assignment: {reloc_line}"));
  let (base_idx, derived_idx) =
    parse_relocate_indices(reloc_line).unwrap_or_else(|| panic!("failed to parse relocate indices: {reloc_line}"));
  assert_eq!(
    (base_idx, derived_idx),
    (0, 0),
    "expected base/derived indices (0,0) for %p relocation, got ({base_idx},{derived_idx})\n\n{func}"
  );

  assert!(
    func.contains(&format!("ret ptr addrspace(1) {relocated_p}")),
    "expected function to return relocated %p value {relocated_p}:\n{func}"
  );
}

#[test]
fn multiple_gc_roots_are_relocated_with_correct_indices() {
  // Common JS runtime scenario: multiple GC pointers are simultaneously live
  // across a safepoint (e.g. multiple locals/temps across a runtime call).
  //
  // Ensure each relocated SSA uses the correct gc-live index (and is then used
  // after the safepoint).
  let before = r#"
  declare void @bar()

  define i8 @test(ptr addrspace(1) %a, ptr addrspace(1) %b) gc "coreclr" {
  entry:
    call void @bar()
    %va = load i8, ptr addrspace(1) %a, align 1
    %vb = load i8, ptr addrspace(1) %b, align 1
    %sum = add i8 %va, %vb
    ret i8 %sum
  }
  "#;

  let after = rewritten_ir(before);
  let func = function_block(&after, "@test");

  let statepoint_line = func
    .lines()
    .find(|l| l.contains("elementtype(void ()) @bar"))
    .unwrap_or_else(|| panic!("missing @bar statepoint call in function:\n{func}"));
  let gc_live = parse_gc_live_vars(statepoint_line);

  assert!(
    gc_live.len() == 2 && gc_live.iter().any(|v| v == "%a") && gc_live.iter().any(|v| v == "%b"),
    "expected gc-live to contain exactly %a and %b, got {gc_live:?}\n\n{func}"
  );

  for var in ["%a", "%b"] {
    let reloc_line = expect_relocate_line(&func, var, var);
    let relocated = assigned_ssa(reloc_line)
      .unwrap_or_else(|| panic!("expected relocate assignment for {var}: {reloc_line}"));
    let (base_idx, derived_idx) = parse_relocate_indices(reloc_line)
      .unwrap_or_else(|| panic!("failed to parse relocate indices: {reloc_line}"));
    let pos = gc_live
      .iter()
      .position(|v| v == var)
      .unwrap_or_else(|| panic!("expected {var} to be present in gc-live vars {gc_live:?}\n\n{func}"));
    assert_eq!(
      (base_idx as usize, derived_idx as usize),
      (pos, pos),
      "expected gc.relocate indices for {var} to be ({pos},{pos}), got ({base_idx},{derived_idx})\n\n{func}"
    );
    assert!(
      func.contains(&format!("load i8, ptr addrspace(1) {relocated}")),
      "expected post-safepoint load to use relocated value {relocated} for {var}:\n{func}"
    );
  }

  // Ensure the original (pre-safepoint) SSA values are not used by the loads.
  assert!(
    !func
      .lines()
      .any(|l| l.contains("load i8, ptr addrspace(1) %a,")),
    "expected loads to not use original %a after safepoint:\n{func}"
  );
  assert!(
    !func
      .lines()
      .any(|l| l.contains("load i8, ptr addrspace(1) %b,")),
    "expected loads to not use original %b after safepoint:\n{func}"
  );
}

#[test]
fn gc_result_pointer_used_across_later_safepoint_is_relocated() {
  // Important real-world pattern: allocate an object (pointer result from a
  // statepoint) and then keep it live across a later safepoint call.
  let before = r#"
  declare ptr addrspace(1) @alloc()
  declare void @bar()

  define i8 @test() gc "coreclr" {
  entry:
    %obj = call ptr addrspace(1) @alloc()
    call void @bar()
    %v = load i8, ptr addrspace(1) %obj
    ret i8 %v
  }
  "#;

  let after = rewritten_ir(before);
  let func = function_block(&after, "@test");

  // Capture the SSA name produced by the allocation result.
  let gc_result_line = func
    .lines()
    .find(|l| l.contains("@llvm.experimental.gc.result.p1"))
    .unwrap_or_else(|| panic!("missing gc.result.p1 in function:\n{func}"));
  let alloc_obj = assigned_ssa(gc_result_line)
    .unwrap_or_else(|| panic!("unexpected gc.result.p1 line: {gc_result_line}"));

  let bar_statepoint_line = func
    .lines()
    .find(|l| l.contains("elementtype(void ()) @bar"))
    .unwrap_or_else(|| panic!("missing @bar statepoint call in function:\n{func}"));
  let gc_live_vars = parse_gc_live_vars(bar_statepoint_line);
  assert!(
    gc_live_vars.iter().any(|v| v == &alloc_obj),
    "expected @bar statepoint gc-live to include alloc result {alloc_obj}, got {gc_live_vars:?}\n\n{func}"
  );

  // Relocation indices must reference the gc-live entry.
  let relocate_line = expect_relocate_line(&func, &alloc_obj, &alloc_obj);
  let relocated_obj = assigned_ssa(relocate_line)
    .unwrap_or_else(|| panic!("expected relocate assignment: {relocate_line}"));
  let (base_idx, derived_idx) = parse_relocate_indices(relocate_line)
    .unwrap_or_else(|| panic!("failed to parse relocate indices: {relocate_line}"));
  let pos = gc_live_vars
    .iter()
    .position(|v| v == &alloc_obj)
    .expect("alloc result should be in gc-live vars");
  assert_eq!(
    base_idx as usize, pos,
    "expected gc.relocate base_idx to equal gc-live position {pos} for {alloc_obj}, got {base_idx}\n\n{func}"
  );
  assert_eq!(
    derived_idx as usize, pos,
    "expected gc.relocate derived_idx to equal gc-live position {pos} for {alloc_obj}, got {derived_idx}\n\n{func}"
  );

  assert!(
    func.contains(&format!("load i8, ptr addrspace(1) {relocated_obj}")),
    "expected post-safepoint load to use relocated obj {relocated_obj}:\n{func}"
  );
}

#[test]
fn derived_pointer_is_rematerialized_from_relocated_base() {
  // LLVM is allowed (and often prefers) to rematerialize simple derived pointers
  // (e.g. `gep base, const`) instead of relocating them directly. In that case,
  // the base pointer must still be kept live across the safepoint and relocated,
  // and the derived pointer must be recomputed from the relocated base.
  let before = r#"
  declare void @bar()

  define i8 @test(ptr addrspace(1) %base) gc "coreclr" {
  entry:
    %derived = getelementptr i8, ptr addrspace(1) %base, i64 8
    call void @bar()
    %v = load i8, ptr addrspace(1) %derived
    ret i8 %v
  }
  "#;

  let after = rewritten_ir(before);
  let func = function_block(&after, "@test");

  let statepoint_line = func
    .lines()
    .find(|l| l.contains("elementtype(void ()) @bar"))
    .unwrap_or_else(|| panic!("missing @bar statepoint call in function:\n{func}"));
  let gc_live_vars = parse_gc_live_vars(statepoint_line);
  assert_eq!(
    gc_live_vars,
    vec!["%base".to_string()],
    "expected gc-live to contain only %base (derived should be rematerialized), got {gc_live_vars:?}\n\n{func}"
  );

  let base_reloc_line = expect_relocate_line(&func, "%base", "%base");
  let base_relocated = assigned_ssa(base_reloc_line)
    .unwrap_or_else(|| panic!("expected relocate assignment: {base_reloc_line}"));

  let remat_line = func
    .lines()
    .find(|l| l.contains("getelementptr") && l.contains(&format!(" {base_relocated}, i64 8")))
    .unwrap_or_else(|| {
      panic!(
        "expected derived pointer to be rematerialized from relocated base {base_relocated}:\n\n{func}"
      )
    });
  let derived_remat = assigned_ssa(remat_line).unwrap_or_else(|| {
    panic!("expected rematerialized derived GEP to be an assignment: {remat_line}")
  });

  assert!(
    func.contains(&format!("load i8, ptr addrspace(1) {derived_remat}")),
    "expected post-safepoint load to use rematerialized derived pointer {derived_remat}:\n{func}"
  );
}

#[test]
fn derived_pointer_relocation_has_distinct_base_and_derived_indices() {
  let before = r#"
 declare void @bar()

define void @test(ptr addrspace(1) %base, i1 %cond) gc "coreclr" {
entry:
  br i1 %cond, label %t, label %f

t:
  %d1 = getelementptr i8, ptr addrspace(1) %base, i64 8
  br label %join

f:
  %d2 = getelementptr i8, ptr addrspace(1) %base, i64 16
  br label %join

join:
  %derived = phi ptr addrspace(1) [ %d1, %t ], [ %d2, %f ]
  call void @bar()
  %a = load i8, ptr addrspace(1) %derived, align 1
  ret void
}
"#;

  let after = rewritten_ir(before);

  let func = function_block(&after, "@test");
  let statepoint_line = func
    .lines()
    .find(|l| l.contains("@llvm.experimental.gc.statepoint"))
    .unwrap_or_else(|| panic!("missing gc.statepoint call in function:\n{func}"));
  assert!(
    statepoint_line.contains("\"gc-live\""),
    "expected statepoint to have a gc-live bundle:\n{statepoint_line}\n\n{func}"
  );
  assert!(
    statepoint_line.contains("ptr addrspace(1) %base")
      && statepoint_line.contains("ptr addrspace(1) %derived"),
    "expected gc-live bundle to include both %base and %derived:\n{statepoint_line}\n\n{func}"
  );
  let gc_live_vars = parse_gc_live_vars(statepoint_line);
  assert!(
    gc_live_vars.iter().any(|v| v == "%base") && gc_live_vars.iter().any(|v| v == "%derived"),
    "expected gc-live vars to include %base and %derived, got: {gc_live_vars:?}\n\n{func}"
  );

  assert!(
    func.contains("@llvm.experimental.gc.relocate.p1"),
    "expected gc.relocate.p1 calls:\n{func}"
  );

  let mut saw_distinct = false;
  let mut derived_relocated_ssa: Option<String> = None;
  let mut derived_reloc_line: Option<String> = None;
  let mut derived_reloc_indices: Option<(u32, u32)> = None;
  let mut derived_reloc_comment: Option<(String, String)> = None;
  for line in func.lines() {
    if let Some((base, derived)) = parse_relocate_indices(line) {
      if base != derived {
        saw_distinct = true;
        derived_relocated_ssa = assigned_ssa(line);
        derived_reloc_line = Some(line.to_string());
        derived_reloc_indices = Some((base, derived));
        derived_reloc_comment = parse_relocate_comment_vars(line);
        break;
      }
    }
  }

  assert!(
    saw_distinct,
    "expected at least one gc.relocate with base_index != derived_index for derived pointer:\n{func}"
  );

  let derived_relocated_ssa =
    derived_relocated_ssa.expect("expected derived relocation to be assigned to an SSA value");
  assert!(
    func.contains(&format!(
      "load i8, ptr addrspace(1) {derived_relocated_ssa}"
    )),
    "expected derived relocated SSA ({derived_relocated_ssa}) to be used after safepoint:\n{func}"
  );

  // Verify the relocate indices refer to the correct gc-live entries (base/derived).
  let (base_idx, derived_idx) =
    derived_reloc_indices.expect("expected to capture base/derived indices for derived relocation");
  let (base_var, derived_var) = derived_reloc_comment.unwrap_or_else(|| {
    panic!(
      "expected derived relocate line to contain a `; (base, derived)` comment, got:\n{}",
      derived_reloc_line.as_deref().unwrap_or("<missing>")
    )
  });
  assert_eq!(
    derived_var,
    "%derived",
    "expected derived relocation to be for %derived, got {derived_var} in:\n{}",
    derived_reloc_line.as_deref().unwrap_or("<missing>")
  );

  let base_pos = gc_live_vars
    .iter()
    .position(|v| v == &base_var)
    .unwrap_or_else(|| {
      panic!("base var {base_var} missing from gc-live vars {gc_live_vars:?}\n\n{func}")
    });
  let derived_pos = gc_live_vars
    .iter()
    .position(|v| v == &derived_var)
    .unwrap_or_else(|| {
      panic!("derived var {derived_var} missing from gc-live vars {gc_live_vars:?}\n\n{func}")
    });
  assert_eq!(
    base_idx as usize, base_pos,
    "gc.relocate base index should point at gc-live[{base_pos}]={base_var}, got base_idx={base_idx}\n\n{func}"
  );
  assert_eq!(
    derived_idx as usize, derived_pos,
    "gc.relocate derived index should point at gc-live[{derived_pos}]={derived_var}, got derived_idx={derived_idx}\n\n{func}"
  );
}

#[test]
fn gc_leaf_function_call_is_not_wrapped_in_statepoint_but_uses_relocated_value() {
  let before = r#"
declare void @bar()
declare void @leaf(ptr addrspace(1)) "gc-leaf-function"

define void @test(ptr addrspace(1) %obj) gc "coreclr" {
entry:
  call void @bar()
  call void @leaf(ptr addrspace(1) %obj)
  ret void
}
"#;

  let after = rewritten_ir(before);
  let func = function_block(&after, "@test");

  assert!(
    func.contains("@llvm.experimental.gc.statepoint"),
    "expected a statepoint for the non-leaf @bar call:\n{func}"
  );

  let leaf_call = func
    .lines()
    .find(|l| l.contains("call void @leaf("))
    .unwrap_or_else(|| panic!("missing direct leaf call:\n{func}"));

  for line in func.lines() {
    assert!(
      !(line.contains("@llvm.experimental.gc.statepoint") && line.contains("@leaf")),
      "leaf call should not be wrapped in a statepoint, but found:\n{line}\n\n{func}"
    );
  }

  let arg = extract_first_arg_after("ptr addrspace(1)", leaf_call)
    .unwrap_or_else(|| panic!("failed to parse leaf call arg from: {leaf_call}"));

  assert!(
    func
      .lines()
      .any(|l| l.contains(&format!("{arg} = call")) && l.contains("@llvm.experimental.gc.relocate.p1")),
    "expected leaf call to use a relocated SSA value ({arg}), but did not find it defined by gc.relocate:\n{func}"
  );
}

#[test]
fn relocation_is_chained_across_multiple_statepoints() {
  // Real-world pattern: a pointer is live across multiple safepoints (e.g.
  // multiple runtime calls). After each statepoint, subsequent safepoints must
  // treat the relocated SSA value as the live root (not the original one).
  let before = r#"
  declare void @bar()
  declare void @baz()

  define i8 @test(ptr addrspace(1) %obj) gc "coreclr" {
  entry:
    call void @bar()
    call void @baz()
    %v = load i8, ptr addrspace(1) %obj
    ret i8 %v
  }
  "#;

  let after = rewritten_ir(before);
  let func = function_block(&after, "@test");

  let bar_statepoint_line = func
    .lines()
    .find(|l| l.contains("elementtype(void ()) @bar"))
    .unwrap_or_else(|| panic!("missing @bar statepoint call in function:\n{func}"));
  let bar_gc_live = parse_gc_live_vars(bar_statepoint_line);
  assert!(
    bar_gc_live == vec!["%obj".to_string()],
    "expected @bar gc-live to be exactly [%obj], got {bar_gc_live:?}\n\n{func}"
  );

  let first_reloc_line = expect_relocate_line(&func, "%obj", "%obj");
  let obj_after_bar = assigned_ssa(first_reloc_line)
    .unwrap_or_else(|| panic!("expected relocate assignment: {first_reloc_line}"));

  let baz_statepoint_line = func
    .lines()
    .find(|l| l.contains("elementtype(void ()) @baz"))
    .unwrap_or_else(|| panic!("missing @baz statepoint call in function:\n{func}"));
  let baz_gc_live = parse_gc_live_vars(baz_statepoint_line);
  assert!(
    baz_gc_live.iter().any(|v| v == &obj_after_bar),
    "expected @baz gc-live to include relocated obj {obj_after_bar}, got {baz_gc_live:?}\n\n{func}"
  );
  assert!(
    !baz_gc_live.iter().any(|v| v == "%obj"),
    "expected @baz gc-live to NOT include original %obj (must use relocated), got {baz_gc_live:?}\n\n{func}"
  );

  let second_reloc_line = expect_relocate_line(&func, &obj_after_bar, &obj_after_bar);
  let obj_after_baz = assigned_ssa(second_reloc_line)
    .unwrap_or_else(|| panic!("expected relocate assignment: {second_reloc_line}"));

  assert!(
    func.contains(&format!("load i8, ptr addrspace(1) {obj_after_baz}")),
    "expected post-safepoint load to use relocated obj {obj_after_baz}:\n{func}"
  );
}

#[test]
fn object_emits_llvm_stackmaps_section() {
  let before = r#"
 declare void @bar()

define void @test(ptr addrspace(1) %base) gc "coreclr" {
entry:
  %derived = getelementptr i8, ptr addrspace(1) %base, i64 8
  call void @bar()
  %a = load i8, ptr addrspace(1) %derived, align 1
  %b = load i8, ptr addrspace(1) %base, align 1
  ret void
}
"#;

  with_rewritten_module(before, |module, tm| {
    let dir = tempdir().expect("create tempdir");
    let obj_path = dir.path().join("statepoint.o");

    tm.write_to_file(module, FileType::Object, &obj_path)
      .unwrap_or_else(|err| panic!("failed to emit object file: {err}"));

    let bytes = fs::read(&obj_path).expect("read emitted object");
    let obj = object::File::parse(&*bytes).expect("parse emitted object");
    assert!(
      obj.section_by_name(".llvm_stackmaps").is_some(),
      "expected .llvm_stackmaps section in emitted object.\nIR:\n{}",
      module.print_to_string()
    );
  });
}
