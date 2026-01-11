use inkwell::context::Context;
use inkwell::memory_buffer::MemoryBuffer;
use inkwell::passes::PassBuilderOptions;
use inkwell::targets::{CodeModel, FileType, InitializationConfig, RelocMode, Target, TargetMachine};
use inkwell::OptimizationLevel;
use std::process::Command;
use std::sync::Once;
use tempfile::tempdir;

static LLVM_INIT: Once = Once::new();

fn init_llvm() {
  LLVM_INIT.call_once(|| {
    Target::initialize_native(&InitializationConfig::default())
      .expect("failed to initialize native LLVM target");
  });
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

  let pb_opts = PassBuilderOptions::create();
  module
    .run_passes("rewrite-statepoints-for-gc", &tm, pb_opts)
    .unwrap_or_else(|err| {
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

  assert!(
    in_func,
    "function {func_name} not found in IR:\n{ir}"
  );

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

  Some((parse_i32_constant(base_part)?, parse_i32_constant(derived_part)?))
}

#[test]
fn gc_result_for_scalar_return() {
  let before = r#"
declare i64 @bar()

define i64 @test() gc "statepoint-example" {
entry:
  %x = call i64 @bar()
  ret i64 %x
}
"#;

  let after = rewritten_ir(before);

  assert!(
    after.contains("@llvm.experimental.gc.statepoint"),
    "missing gc.statepoint intrinsic:\n{after}"
  );
  assert!(
    after.contains("@llvm.experimental.gc.result.i64"),
    "missing gc.result.i64 intrinsic:\n{after}"
  );

  let func = function_block(&after, "@test");
  assert!(
    !func.contains("call i64 @bar"),
    "expected @bar call to be rewritten (no direct call i64 @bar):\n{func}"
  );

  let gc_result_line = func
    .lines()
    .find(|l| l.contains("@llvm.experimental.gc.result.i64"))
    .unwrap_or_else(|| panic!("missing gc.result in function:\n{func}"));
  let result_ssa =
    assigned_ssa(gc_result_line).unwrap_or_else(|| panic!("unexpected gc.result line: {gc_result_line}"));

  assert!(
    func.contains(&format!("ret i64 {result_ssa}")),
    "expected function to return gc.result value ({result_ssa}):\n{func}"
  );
}

#[test]
fn gc_result_for_gc_pointer_return() {
  let before = r#"
declare ptr addrspace(1) @alloc()

define ptr addrspace(1) @test() gc "statepoint-example" {
entry:
  %p = call ptr addrspace(1) @alloc()
  ret ptr addrspace(1) %p
}
"#;

  let after = rewritten_ir(before);

  assert!(
    after.contains("@llvm.experimental.gc.statepoint"),
    "missing gc.statepoint intrinsic:\n{after}"
  );
  assert!(
    after.contains("@llvm.experimental.gc.result.p1"),
    "missing gc.result.p1 intrinsic:\n{after}"
  );

  let func = function_block(&after, "@test");
  assert!(
    func.contains("ret ptr addrspace(1) %"),
    "expected return type to remain ptr addrspace(1):\n{func}"
  );

  let gc_result_line = func
    .lines()
    .find(|l| l.contains("@llvm.experimental.gc.result.p1"))
    .unwrap_or_else(|| panic!("missing gc.result.p1 in function:\n{func}"));
  let result_ssa =
    assigned_ssa(gc_result_line).unwrap_or_else(|| panic!("unexpected gc.result line: {gc_result_line}"));

  assert!(
    func.contains(&format!("ret ptr addrspace(1) {result_ssa}")),
    "expected function to return gc.result value ({result_ssa}):\n{func}"
  );
}

#[test]
fn derived_pointer_relocation_has_distinct_base_and_derived_indices() {
  let before = r#"
declare void @bar()

define void @test(ptr addrspace(1) %base, i1 %cond) gc "statepoint-example" {
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

  assert!(
    after.contains("\"gc-live\""),
    "expected statepoint to have a gc-live bundle:\n{after}"
  );

  let func = function_block(&after, "@test");
  assert!(
    func.contains("@llvm.experimental.gc.relocate.p1"),
    "expected gc.relocate.p1 calls:\n{func}"
  );

  let mut saw_distinct = false;
  for line in func.lines() {
    if let Some((base, derived)) = parse_relocate_indices(line) {
      if base != derived {
        saw_distinct = true;
        break;
      }
    }
  }

  assert!(
    saw_distinct,
    "expected at least one gc.relocate with base_index != derived_index for derived pointer:\n{func}"
  );
}

#[test]
fn gc_leaf_function_call_is_not_wrapped_in_statepoint_but_uses_relocated_value() {
  let before = r#"
declare void @bar()
declare void @leaf(ptr addrspace(1)) "gc-leaf-function"

define void @test(ptr addrspace(1) %obj) gc "statepoint-example" {
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
fn object_emits_llvm_stackmaps_section() {
  let before = r#"
declare void @bar()

define void @test(ptr addrspace(1) %base) gc "statepoint-example" {
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

    let output = Command::new("llvm-readobj-18")
      .arg("--sections")
      .arg(&obj_path)
      .output()
      .unwrap_or_else(|err| {
        panic!(
          "failed to run llvm-readobj-18: {err}\n\
           ensure LLVM 18 is installed and llvm-readobj-18 is in PATH"
        )
      });

    assert!(
      output.status.success(),
      "llvm-readobj-18 failed with status {}:\nstdout:\n{}\nstderr:\n{}",
      output.status,
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
      stdout.contains(".llvm_stackmaps") || stderr.contains(".llvm_stackmaps"),
      "expected .llvm_stackmaps section in emitted object.\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
  });
}
