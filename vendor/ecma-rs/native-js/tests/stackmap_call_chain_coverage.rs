//! Regression tests for "older frame" stackmap coverage.
//!
//! At GC time, only the top frame is stopped at a safepoint. Older frames are suspended at their
//! **callsite return addresses**. If any TS→TS callsite is lowered as a plain `call` (not a
//! statepoint), that return PC will not map to a stackmap record and precise stack scanning becomes
//! unsound.

#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

use inkwell::attributes::AttributeLoc;
use inkwell::context::Context;
use inkwell::targets::{CodeModel, RelocMode, Target, TargetMachine};
use inkwell::OptimizationLevel;
use native_js::llvm::passes;
use object::{Object, ObjectSection, ObjectSymbol};
use runtime_native::stackmap_loader;
use runtime_native::stackmaps::StackMaps;

fn cmd_works(cmd: &str) -> bool {
  Command::new(cmd)
    .arg("--version")
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status()
    .is_ok_and(|s| s.success())
}

fn clang_available() -> bool {
  cmd_works("clang-18") || cmd_works("clang")
}

fn lld_available() -> bool {
  cmd_works("ld.lld-18") || cmd_works("ld.lld")
}

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

fn find_symbol<'data>(file: &'data object::File<'data>, name: &str) -> object::Symbol<'data, 'data> {
  file
    .symbols()
    .find(|s| s.name().ok() == Some(name))
    .unwrap_or_else(|| panic!("missing symbol `{name}`"))
}

fn call_return_address_x86_64(
  file: &object::File<'_>,
  caller_sym: object::Symbol<'_, '_>,
  callee_addr: u64,
) -> u64 {
  let section_idx = caller_sym
    .section_index()
    .unwrap_or_else(|| panic!("symbol {} has no section index", caller_sym.name().unwrap()));
  let section = file.section_by_index(section_idx).expect("caller section");
  let section_addr = section.address();
  let data = section.data().expect("caller section data");

  let func_start = caller_sym.address();
  let func_size = caller_sym.size();
  assert!(func_size > 0, "caller symbol has size 0");

  let start_off = (func_start - section_addr) as usize;
  let end_off = start_off + func_size as usize;
  let body = &data[start_off..end_off];

  // Scan for `E8 rel32` calls targeting `callee_addr`.
  for i in 0..body.len().saturating_sub(5) {
    if body[i] != 0xE8 {
      continue;
    }
    let rel = i32::from_le_bytes(body[i + 1..i + 5].try_into().unwrap()) as i64;
    let call_addr = func_start + i as u64;
    let ret_addr = call_addr + 5;
    let target = (ret_addr as i64 + rel) as u64;
    if target == callee_addr {
      return ret_addr;
    }
  }

  panic!(
    "failed to find x86_64 direct call from {} to callee at 0x{callee_addr:x}",
    caller_sym.name().unwrap()
  );
}

fn write_file(path: &Path, bytes: &[u8]) {
  fs::write(path, bytes).unwrap_or_else(|e| panic!("failed to write {}: {e}", path.display()));
}

#[test]
fn deep_ts_call_chain_callsite_pcs_are_present_in_stackmaps() {
  if !clang_available() {
    eprintln!("skipping: clang not found in PATH (expected `clang-18` or `clang`)");
    return;
  }
  if !lld_available() {
    eprintln!("skipping: lld not found in PATH (expected `ld.lld-18` or `ld.lld`)");
    return;
  }

  let tm = host_target_machine();
  let context = Context::create();
  let module = context.create_module("stackmap_call_chain");
  let builder = context.create_builder();

  // Mark TS functions using native-js's stable naming scheme so the post-rewrite verifier applies.
  let f0_name = "__nativejs_def_0000000000000000_f0";
  let f1_name = "__nativejs_def_0000000000000001_f1";
  let f2_name = "__nativejs_def_0000000000000002_f2";

  let void_ty = context.void_type();
  let fn_ty = void_ty.fn_type(&[], false);

  let f0 = module.add_function(f0_name, fn_ty, None);
  let f1 = module.add_function(f1_name, fn_ty, None);
  let f2 = module.add_function(f2_name, fn_ty, None);
  let safepoint = module.add_function("safepoint", fn_ty, None);
  let main_ty = context.i32_type().fn_type(&[], false);
  let main = module.add_function("main", main_ty, None);

  for f in [f0, f1, f2] {
    native_js::llvm::gc::set_default_gc_strategy(&f).expect("gc strategy");

    // Keep stack walking deterministic (matches native-js policy).
    let frame_pointer = context.create_string_attribute("frame-pointer", "all");
    let disable_tail_calls = context.create_string_attribute("disable-tail-calls", "true");
    f.add_attribute(AttributeLoc::Function, frame_pointer);
    f.add_attribute(AttributeLoc::Function, disable_tail_calls);
  }

  // safepoint is a normal function; it doesn't need GC strategy for this test.
  let entry = context.append_basic_block(safepoint, "entry");
  builder.position_at_end(entry);
  builder.build_return(None).unwrap();

  let entry = context.append_basic_block(f2, "entry");
  builder.position_at_end(entry);
  builder.build_call(safepoint, &[], "").unwrap();
  builder.build_return(None).unwrap();

  let entry = context.append_basic_block(f1, "entry");
  builder.position_at_end(entry);
  builder.build_call(f2, &[], "").unwrap();
  builder.build_return(None).unwrap();

  let entry = context.append_basic_block(f0, "entry");
  builder.position_at_end(entry);
  builder.build_call(f1, &[], "").unwrap();
  builder.build_return(None).unwrap();

  let entry = context.append_basic_block(main, "entry");
  builder.position_at_end(entry);
  builder.build_call(f0, &[], "").unwrap();
  let zero = context.i32_type().const_int(0, false);
  builder.build_return(Some(&zero)).unwrap();

  module.set_triple(&tm.get_triple());
  module.set_data_layout(&tm.get_target_data().get_data_layout());

  // Runs `rewrite-statepoints-for-gc` and the native-js callsite invariant verifier.
  passes::rewrite_statepoints_for_gc(&module, &tm).unwrap_or_else(|err| {
    panic!(
      "rewrite-statepoints-for-gc failed: {err}\n\nBefore/After IR:\n{}",
      module.print_to_string()
    )
  });

  if let Err(err) = module.verify() {
    panic!("module verification failed: {err}\n\nIR:\n{}", module.print_to_string());
  }

  // Emit object + link into a non-PIE executable so stackmap PCs are absolute.
  let obj = tm
    .write_to_memory_buffer(&module, inkwell::targets::FileType::Object)
    .expect("emit object")
    .as_slice()
    .to_vec();

  let td = tempfile::tempdir().unwrap();
  let obj_path = td.path().join("out.o");
  write_file(&obj_path, &obj);

  let exe_path = td.path().join("a.out");
  native_js::link::link_elf_executable(&exe_path, &[obj_path.clone()]).unwrap();

  let exe = fs::read(&exe_path).unwrap();
  let file = object::File::parse(&*exe).expect("parse linked executable");

  // Derive the callsite return PCs for:
  //   f0 -> f1
  //   f1 -> f2
  let f0_sym = find_symbol(&file, f0_name);
  let f1_sym = find_symbol(&file, f1_name);
  let f2_sym = find_symbol(&file, f2_name);
  let f0_to_f1_ret = call_return_address_x86_64(&file, f0_sym, f1_sym.address());
  let f1_to_f2_ret = call_return_address_x86_64(&file, f1_sym, f2_sym.address());

  let stackmaps_section = stackmap_loader::find_stackmap_section(&exe)
    .expect("find stackmaps section")
    .expect("missing stackmaps section (was it GC'd?)");
  let maps = StackMaps::parse(stackmaps_section.bytes).expect("parse stackmaps");

  assert!(
    maps.lookup(f0_to_f1_ret).is_some(),
    "missing stackmap record for f0->f1 callsite return PC 0x{f0_to_f1_ret:x}"
  );
  assert!(
    maps.lookup(f1_to_f2_ret).is_some(),
    "missing stackmap record for f1->f2 callsite return PC 0x{f1_to_f2_ret:x}"
  );
}

#[test]
fn stray_plain_call_in_ts_function_is_rejected() {
  let tm = host_target_machine();
  let context = Context::create();
  let module = context.create_module("stackmap_stray_call_rejected");
  let builder = context.create_builder();

  let void_ty = context.void_type();
  let fn_ty = void_ty.fn_type(&[], false);

  // TS-generated name, but intentionally missing `gc "coreclr"` so rewrite-statepoints-for-gc
  // won't rewrite its calls.
  let caller_name = "__nativejs_def_0000000000000000_stray";
  let callee_name = "__nativejs_def_0000000000000001_callee";
  let caller = module.add_function(caller_name, fn_ty, None);
  let callee = module.add_function(callee_name, fn_ty, None);

  let entry = context.append_basic_block(callee, "entry");
  builder.position_at_end(entry);
  builder.build_return(None).unwrap();

  let entry = context.append_basic_block(caller, "entry");
  builder.position_at_end(entry);
  builder.build_call(callee, &[], "").unwrap();
  builder.build_return(None).unwrap();

  module.set_triple(&tm.get_triple());
  module.set_data_layout(&tm.get_target_data().get_data_layout());

  let err = passes::rewrite_statepoints_for_gc(&module, &tm).expect_err("expected verifier failure");
  let msg = err.to_string();
  assert!(
    msg.contains(caller_name),
    "error message should mention offending function; got: {msg}"
  );
}
