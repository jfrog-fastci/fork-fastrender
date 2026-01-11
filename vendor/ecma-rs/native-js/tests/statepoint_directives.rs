use inkwell::context::Context;
use inkwell::targets::{CodeModel, FileType, InitializationConfig, RelocMode, Target, TargetMachine};
use inkwell::values::AsValueRef;
use inkwell::OptimizationLevel;
use native_js::llvm::{gc, passes, statepoint_directives};
use object::{Object, ObjectSection};
use runtime_native::stackmap::StackMap;
use std::process::{Command, Stdio};
use std::sync::Once;
use tempfile::tempdir;

static LLVM_INIT: Once = Once::new();

fn host_target_machine() -> TargetMachine {
  LLVM_INIT.call_once(|| {
    Target::initialize_native(&InitializationConfig::default()).expect("failed to init native target");
  });

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

fn llvm_objdump() -> Option<&'static str> {
  for candidate in ["llvm-objdump-18", "llvm-objdump"] {
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

fn parse_objdump_function<'a>(objdump: &'a str, func_name: &str) -> Option<(u64, Vec<(u64, &'a str)>)> {
  let mut in_func = false;
  let mut fn_start: u64 = 0;
  let mut insts = Vec::new();

  for line in objdump.lines() {
    if !in_func {
      let needle = format!("<{func_name}>:");
      if line.contains(&needle) {
        let addr_str = line.split_whitespace().next()?;
        fn_start = u64::from_str_radix(addr_str, 16).ok()?;
        in_func = true;
      }
      continue;
    }

    // Stop at the next function header.
    if line.trim_end().ends_with(">:") && line.contains(" <") && !line.contains(&format!("<{func_name}>:")) {
      break;
    }

    let trimmed = line.trim();
    let Some((addr_str, rest)) = trimmed.split_once(':') else {
      continue;
    };
    let addr_str = addr_str.trim();
    if addr_str.is_empty() || !addr_str.chars().all(|c| c.is_ascii_hexdigit()) {
      continue;
    }
    let addr = u64::from_str_radix(addr_str, 16).ok()?;
    let mnemonic = rest.trim().split_whitespace().next().unwrap_or("");
    if !mnemonic.is_empty() {
      insts.push((addr, mnemonic));
    }
  }

  if in_func {
    Some((fn_start, insts))
  } else {
    None
  }
}

#[test]
fn rewrite_statepoints_honors_callsite_directives() {
  let context = Context::create();
  let module = context.create_module("statepoint_directives");
  let builder = context.create_builder();

  let void_ty = context.void_type();
  let bar_ty = void_ty.fn_type(&[], false);
  let bar = module.add_function("bar", bar_ty, None);

  let foo_ty = void_ty.fn_type(&[], false);
  let foo = module.add_function("foo", foo_ty, None);
  // `rewrite-statepoints-for-gc` only rewrites callsites in functions marked with a GC strategy.
  gc::set_default_gc_strategy(&foo).expect("GC strategy contains NUL byte");

  let entry = context.append_basic_block(foo, "entry");
  builder.position_at_end(entry);

  let call = builder.build_call(bar, &[], "call_bar").expect("build call");
  statepoint_directives::set_callsite_statepoint_id(call.as_value_ref(), 42);
  statepoint_directives::set_callsite_statepoint_num_patch_bytes(call.as_value_ref(), 16);
  builder.build_return(None).expect("build return");

  let tm = host_target_machine();
  module.set_triple(&tm.get_triple());
  module.set_data_layout(&tm.get_target_data().get_data_layout());

  passes::rewrite_statepoints_for_gc(&module, &tm).expect("rewrite-statepoints-for-gc failed");

  let ir = module.print_to_string().to_string();
  assert!(
    ir.contains("@llvm.experimental.gc.statepoint.p0(i64 42, i32 16"),
    "expected statepoint id/patch-bytes in rewritten IR, got:\n{ir}"
  );

  // Stronger check: the statepoint ID is the StackMap patchpoint ID (encoded as a u64 in
  // `.llvm_stackmaps`).
  let tmp = tempdir().expect("failed to create tempdir");
  let obj = tmp.path().join("statepoints.o");
  tm.write_to_file(&module, FileType::Object, &obj)
    .expect("failed to emit object file");

  let data = std::fs::read(&obj).expect("read emitted object");
  let file = object::File::parse(&*data).expect("parse emitted object");
  let section = file
    .section_by_name(".llvm_stackmaps")
    .expect("missing .llvm_stackmaps section");
  let stackmaps = StackMap::parse(section.data().expect("read .llvm_stackmaps section bytes"))
    .expect("parse .llvm_stackmaps");

  assert_eq!(
    stackmaps.records.len(),
    1,
    "expected exactly 1 stackmap record, got {}\nIR:\n{ir}",
    stackmaps.records.len()
  );
  assert_eq!(
    stackmaps.records[0].patchpoint_id, 42,
    "expected stackmap patchpoint_id to match statepoint-id=42\nIR:\n{ir}"
  );
}

#[test]
fn rewrite_statepoints_uses_default_statepoint_id_when_unset() {
  let context = Context::create();
  let module = context.create_module("statepoint_default_id");
  let builder = context.create_builder();

  let void_ty = context.void_type();
  let bar_ty = void_ty.fn_type(&[], false);
  let bar = module.add_function("bar", bar_ty, None);

  let foo_ty = void_ty.fn_type(&[], false);
  let foo = module.add_function("foo", foo_ty, None);
  gc::set_default_gc_strategy(&foo).expect("GC strategy contains NUL byte");

  let entry = context.append_basic_block(foo, "entry");
  builder.position_at_end(entry);
  builder.build_call(bar, &[], "call_bar").expect("build call");
  builder.build_return(None).expect("build return");

  let tm = host_target_machine();
  module.set_triple(&tm.get_triple());
  module.set_data_layout(&tm.get_target_data().get_data_layout());

  passes::rewrite_statepoints_for_gc(&module, &tm).expect("rewrite-statepoints-for-gc failed");

  let ir = module.print_to_string().to_string();
  assert!(
    ir.contains("@llvm.experimental.gc.statepoint.p0(i64 2882400000, i32 0"),
    "expected default statepoint id/patch-bytes in rewritten IR, got:\n{ir}"
  );

  // The default statepoint ID is also used as the StackMap record patchpoint ID.
  let tmp = tempdir().expect("failed to create tempdir");
  let obj = tmp.path().join("statepoints.o");
  tm.write_to_file(&module, FileType::Object, &obj)
    .expect("failed to emit object file");

  let data = std::fs::read(&obj).expect("read emitted object");
  let file = object::File::parse(&*data).expect("parse emitted object");
  let section = file
    .section_by_name(".llvm_stackmaps")
    .expect("missing .llvm_stackmaps section");
  let stackmaps = StackMap::parse(section.data().expect("read .llvm_stackmaps section bytes"))
    .expect("parse .llvm_stackmaps");

  assert_eq!(
    stackmaps.records.len(),
    1,
    "expected exactly 1 stackmap record, got {}\nIR:\n{ir}",
    stackmaps.records.len()
  );
  assert_eq!(
    stackmaps.records[0].patchpoint_id, 2882400000,
    "expected default StackMap patchpoint_id to be 2882400000 (0xABCDEF00)\nIR:\n{ir}"
  );
}

#[test]
fn statepoint_num_patch_bytes_reserves_nop_sled_x86_64() {
  if !cfg!(all(target_os = "linux", target_arch = "x86_64")) {
    eprintln!("skipping: patch-bytes NOP sled shape is only tested on linux x86_64");
    return;
  }

  let Some(objdump_bin) = llvm_objdump() else {
    eprintln!("skipping: llvm-objdump not found");
    return;
  };

  let context = Context::create();
  let module = context.create_module("statepoint_patch_bytes");
  let builder = context.create_builder();

  let void_ty = context.void_type();
  let bar_ty = void_ty.fn_type(&[], false);
  let bar = module.add_function("bar", bar_ty, None);

  let foo_ty = void_ty.fn_type(&[], false);
  let foo = module.add_function("foo", foo_ty, None);
  gc::set_gc_strategy(&foo, "statepoint-example").expect("GC strategy contains NUL byte");

  let entry = context.append_basic_block(foo, "entry");
  builder.position_at_end(entry);

  let call = builder.build_call(bar, &[], "call_bar").expect("build call");
  statepoint_directives::set_callsite_statepoint_id(call.as_value_ref(), 7);
  statepoint_directives::set_callsite_statepoint_num_patch_bytes(call.as_value_ref(), 16);
  builder.build_return(None).expect("build return");

  let tm = host_target_machine();
  module.set_triple(&tm.get_triple());
  module.set_data_layout(&tm.get_target_data().get_data_layout());

  passes::rewrite_statepoints_for_gc(&module, &tm).expect("rewrite-statepoints-for-gc failed");
  let ir = module.print_to_string().to_string();
  assert!(
    ir.contains("@llvm.experimental.gc.statepoint.p0(i64 7, i32 16"),
    "expected rewritten statepoint to use patch-bytes=16, got:\n{ir}"
  );

  let tmp = tempdir().expect("failed to create tempdir");
  let obj = tmp.path().join("statepoints.o");
  tm.write_to_file(&module, FileType::Object, &obj)
    .expect("failed to emit object file");

  // Stackmap instruction offset should point to the end of the patchable region.
  let data = std::fs::read(&obj).expect("read emitted object");
  let file = object::File::parse(&*data).expect("parse emitted object");
  let section = file
    .section_by_name(".llvm_stackmaps")
    .expect("missing .llvm_stackmaps section");
  let stackmaps = StackMap::parse(section.data().expect("read .llvm_stackmaps section bytes"))
    .expect("parse .llvm_stackmaps");
  assert_eq!(stackmaps.records.len(), 1, "expected exactly 1 stackmap record");
  let instr_off = stackmaps.records[0].instruction_offset as u64;

  // Disassemble and locate the NOP run that ends at `FunctionStart + instr_off`.
  let out = Command::new(objdump_bin)
    .arg("-d")
    .arg("--no-show-raw-insn")
    .arg(&obj)
    .output()
    .expect("run llvm-objdump");
  assert!(
    out.status.success(),
    "llvm-objdump failed:\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&out.stdout),
    String::from_utf8_lossy(&out.stderr)
  );
  let disasm = String::from_utf8_lossy(&out.stdout);
  let (fn_start, insts) =
    parse_objdump_function(&disasm, "foo").unwrap_or_else(|| panic!("failed to parse foo disassembly:\n{disasm}"));
  assert!(!insts.is_empty(), "expected foo to have at least one instruction");
  let expected_ret_addr = fn_start + instr_off;

  let idx = insts
    .iter()
    .position(|(addr, _mn)| *addr == expected_ret_addr)
    .unwrap_or_else(|| {
      panic!(
        "failed to find instruction at stackmap return address 0x{expected_ret_addr:x} (offset {instr_off}) in:\n{disasm}"
      )
    });
  assert!(idx > 0, "expected at least one instruction before the return address");
  assert!(
    !insts[idx].1.starts_with("nop"),
    "expected the return-address instruction to be non-NOP, got {}\n{disasm}",
    insts[idx].1
  );

  // Walk backwards to find a contiguous run of `nop*` instructions directly before the return address.
  let mut j = idx - 1;
  while insts[j].1.starts_with("nop") {
    if j == 0 {
      break;
    }
    if !insts[j - 1].1.starts_with("nop") {
      break;
    }
    j -= 1;
  }

  assert!(
    insts[j].1.starts_with("nop"),
    "expected a NOP sled immediately before the return address, got:\n{disasm}"
  );

  let nop_start = insts[j].0;
  let nop_len = expected_ret_addr
    .checked_sub(nop_start)
    .expect("NOP start should be before return address");
  assert_eq!(
    nop_len, 16,
    "expected a 16-byte patchable NOP sled before return address, got len={nop_len}\n{disasm}"
  );

  assert!(
    insts.iter().all(|(_addr, m)| !m.starts_with("call")),
    "expected patch_bytes>0 to suppress direct call emission (should be a NOP sled), got:\n{disasm}"
  );
}
