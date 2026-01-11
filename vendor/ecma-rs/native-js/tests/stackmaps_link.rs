#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::{fs, process::Command};

use anyhow::{anyhow, Context as _, Result};
use inkwell::context::Context;
use inkwell::IntPredicate;
use inkwell::targets::{CodeModel, RelocMode};
use inkwell::OptimizationLevel;
use object::{Object as _, ObjectSection as _, ObjectSymbol as _};

use native_js::link::{LinkOpts, LLVM_STACKMAPS_START_SYM, LLVM_STACKMAPS_STOP_SYM};
use native_js::{emit, llvm::gc};

const STACKMAP_RELOC_SECTION_CANDIDATES: [&str; 4] = [
  ".rela.llvm_stackmaps",
  ".rel.llvm_stackmaps",
  ".rela.data.rel.ro.llvm_stackmaps",
  ".rel.data.rel.ro.llvm_stackmaps",
];

fn assert_stackmaps_present_and_parseable(exe: &[u8]) -> Result<()> {
  let bytes = llvm_stackmaps::elf::stackmaps_section_bytes(exe)
    .map_err(|e| anyhow!("failed to extract stackmaps from ELF: {e}"))?;
  if bytes.is_empty() {
    return Err(anyhow!("expected non-empty stackmaps range"));
  }
  llvm_stackmaps::StackMaps::parse(bytes)
    .map(|_| ())
    .map_err(|e| anyhow!("failed to parse stackmaps bytes: {e}"))
}

fn elf64_le_has_wx_load_segment(bytes: &[u8]) -> Result<bool> {
  // Minimal ELF64 little-endian program header scan.
  //
  // We intentionally parse the ELF header directly instead of relying on external tools like
  // `readelf`, since native-js tests should run in minimal environments.
  if bytes.len() < 64 {
    return Err(anyhow!("ELF header too small ({} bytes)", bytes.len()));
  }
  if &bytes[0..4] != b"\x7fELF" {
    return Err(anyhow!("not an ELF file (bad magic)"));
  }
  // EI_CLASS = 2 (ELFCLASS64), EI_DATA = 1 (ELFDATA2LSB).
  if bytes[4] != 2 {
    return Err(anyhow!("expected ELF64 (EI_CLASS=2), got {}", bytes[4]));
  }
  if bytes[5] != 1 {
    return Err(anyhow!(
      "expected little-endian ELF (EI_DATA=1), got {}",
      bytes[5]
    ));
  }

  let e_phoff = u64::from_le_bytes(bytes[32..40].try_into().unwrap()) as usize;
  let e_phentsize = u16::from_le_bytes(bytes[54..56].try_into().unwrap()) as usize;
  let e_phnum = u16::from_le_bytes(bytes[56..58].try_into().unwrap()) as usize;

  if e_phoff == 0 || e_phnum == 0 {
    return Ok(false);
  }
  if e_phentsize < 8 {
    return Err(anyhow!("ELF program header entry size too small: {e_phentsize}"));
  }
  if e_phoff + e_phentsize * e_phnum > bytes.len() {
    return Err(anyhow!("ELF program header table is out of bounds"));
  }

  for idx in 0..e_phnum {
    let off = e_phoff + idx * e_phentsize;
    let p_type = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap());
    let p_flags = u32::from_le_bytes(bytes[off + 4..off + 8].try_into().unwrap());
    const PT_LOAD: u32 = 1;
    const PF_X: u32 = 0x1;
    const PF_W: u32 = 0x2;
    if p_type == PT_LOAD && (p_flags & PF_X) != 0 && (p_flags & PF_W) != 0 {
      return Ok(true);
    }
  }

  Ok(false)
}

fn find_symbol_addr<'data>(file: &object::File<'data>, name: &str) -> Option<u64> {
  for sym in file.symbols() {
    if sym.name().ok() == Some(name) {
      return Some(sym.address());
    }
  }
  for sym in file.dynamic_symbols() {
    if sym.name().ok() == Some(name) {
      return Some(sym.address());
    }
  }
  None
}

fn assert_exported_stackmaps_range_non_empty(bytes: &[u8]) -> Result<()> {
  let file = object::File::parse(bytes).context("parse object/elf")?;
  let start = find_symbol_addr(&file, LLVM_STACKMAPS_START_SYM)
    .ok_or_else(|| anyhow!("missing {LLVM_STACKMAPS_START_SYM} symbol"))?;
  let end = find_symbol_addr(&file, LLVM_STACKMAPS_STOP_SYM)
    .ok_or_else(|| anyhow!("missing {LLVM_STACKMAPS_STOP_SYM} symbol"))?;

  if end <= start {
    return Err(anyhow!(
      "expected {LLVM_STACKMAPS_STOP_SYM} > {LLVM_STACKMAPS_START_SYM} (start={start:#x}, end={end:#x})"
    ));
  }
  Ok(())
}

/// End-to-end test: generate an object file that contains `.llvm_stackmaps`,
/// link it into an executable, and ensure the final binary keeps the stackmaps
/// data and exports the `__start_llvm_stackmaps`/`__stop_llvm_stackmaps` boundary
/// symbols (even when the stackmaps payload is merged into a broader RELRO
/// section under lld).
///
/// This is a regression test for PIE linking: when building a PIE, `.llvm_stackmaps`
/// can require runtime relocations which often triggers `DT_TEXTREL` warnings.
#[test]
fn link_preserves_llvm_stackmaps_without_reloc_section() -> Result<()> {
  if !clang_available() {
    eprintln!("skipping: clang not found in PATH (expected `clang-18` or `clang`)");
    return Ok(());
  }
  if !lld_available() {
    eprintln!("skipping: lld not found in PATH (expected `ld.lld-18` or `ld.lld`)");
    return Ok(());
  }
  if !command_works("llvm-objcopy-18") && !command_works("llvm-objcopy") {
    eprintln!("skipping: llvm-objcopy not found in PATH (needed to link stackmaps under lld)");
    return Ok(());
  }

  let obj_bytes = build_statepoint_object().context("build statepoint object")?;

  assert_section_present_non_empty(&obj_bytes, ".llvm_stackmaps")?;
  assert_any_section_present_non_empty(
    &obj_bytes,
    &[".rela.llvm_stackmaps", ".rel.llvm_stackmaps"],
  )?;

  let tmp = tempfile::tempdir().context("create tempdir")?;
  let exe_path = tmp.path().join("poc_exe");

  native_js::link::link_object_buffers_to_elf_executable(
    &exe_path,
    &[obj_bytes.as_slice()],
    LinkOpts::default(),
  )?;

  let exe_bytes = fs::read(&exe_path).context("read linked executable")?;
  // `LinkOpts::default()` should be non-PIE on Linux (ET_EXEC).
  let elf_type = u16::from_le_bytes([exe_bytes[16], exe_bytes[17]]);
  assert_eq!(elf_type, 2, "expected non-PIE ET_EXEC (e_type={elf_type})");

  assert_stackmaps_present_and_parseable(&exe_bytes)?;
  assert_exported_stackmaps_range_non_empty(&exe_bytes)?;
  for name in STACKMAP_RELOC_SECTION_CANDIDATES {
    assert_section_absent(&exe_bytes, name)?;
  }

  // Optional: stripping should not remove the allocated stackmaps section.
  if command_works("strip") {
    run(Command::new("strip").arg(&exe_path)).context("strip")?;
    let stripped = fs::read(&exe_path).context("read stripped executable")?;
    for name in STACKMAP_RELOC_SECTION_CANDIDATES {
      assert_section_absent(&stripped, name)?;
    }
  }

  let status = Command::new(&exe_path)
    .status()
    .with_context(|| format!("run {}", exe_path.display()))?;
  if !status.success() {
    return Err(anyhow!("linked executable failed with status {status}"));
  }

  Ok(())
}

#[test]
fn link_pie_without_textrel_keeps_llvm_stackmaps() -> Result<()> {
  if !clang_available() {
    eprintln!("skipping: clang not found in PATH (expected `clang-18` or `clang`)");
    return Ok(());
  }
  if !lld_available() {
    eprintln!("skipping: lld not found in PATH (expected `ld.lld-18` or `ld.lld`)");
    return Ok(());
  }

  if !command_works("llvm-objcopy-18") && !command_works("llvm-objcopy") {
    eprintln!("skipping: llvm-objcopy not found in PATH (needed for PIE stackmaps patching)");
    return Ok(());
  }

  let obj_bytes = build_statepoint_object().context("build statepoint object")?;

  let tmp = tempfile::tempdir().context("create tempdir")?;
  let exe_path = tmp.path().join("poc_pie");

  native_js::link::link_object_buffers_to_elf_executable(
    &exe_path,
    &[obj_bytes.as_slice()],
    LinkOpts {
      pie: true,
      ..Default::default()
    },
  )?;

  let exe_bytes = fs::read(&exe_path).context("read linked executable")?;
  // PIE should be ET_DYN.
  let elf_type = u16::from_le_bytes([exe_bytes[16], exe_bytes[17]]);
  assert_eq!(elf_type, 3, "expected PIE ET_DYN (e_type={elf_type})");

  assert_exported_stackmaps_range_non_empty(&exe_bytes)?;
  assert_stackmaps_present_and_parseable(&exe_bytes)?;
  for name in STACKMAP_RELOC_SECTION_CANDIDATES {
    assert_section_absent(&exe_bytes, name)?;
  }
  assert_no_textrel_dynamic_tags(&exe_bytes)?;
  assert!(
    !elf64_le_has_wx_load_segment(&exe_bytes)?,
    "unexpected RWX PT_LOAD segment in linked PIE executable"
  );

  let status = Command::new(&exe_path)
    .status()
    .with_context(|| format!("run {}", exe_path.display()))?;
  if !status.success() {
    return Err(anyhow!("linked PIE executable failed with status {status}"));
  }

  Ok(())
}

#[test]
fn link_object_to_executable_keeps_stackmaps_under_gc_sections() -> Result<()> {
    if !clang_available() {
        eprintln!("skipping: clang not found in PATH (expected `clang-18` or `clang`)");
        return Ok(());
    }

    let obj_bytes = build_statepoint_object().context("build statepoint object")?;
    assert_section_present_non_empty(&obj_bytes, ".llvm_stackmaps")?;

    let tmp = tempfile::tempdir().context("create tempdir")?;
    let obj_path = tmp.path().join("poc.o");
    let exe_path = tmp.path().join("poc_exe");
    fs::write(&obj_path, &obj_bytes).context("write object file")?;

    native_js::link::link_object_to_executable(&obj_path, &exe_path)
        .map_err(|err| anyhow!("link_object_to_executable failed: {err}"))?;
  
    let exe_bytes = fs::read(&exe_path).context("read linked executable")?;
    assert_stackmaps_present_and_parseable(&exe_bytes)?;

    let status = Command::new(&exe_path)
      .status()
      .with_context(|| format!("run {}", exe_path.display()))?;
    if !status.success() {
      return Err(anyhow!("linked executable failed with status {status}"));
    }

     Ok(())
  }

fn command_works(cmd: &str) -> bool {
  Command::new(cmd)
    .arg("--version")
    .output()
    .map(|o| o.status.success())
    .unwrap_or(false)
}

fn clang_available() -> bool {
  command_works("clang-18") || command_works("clang")
}

fn lld_available() -> bool {
  command_works("ld.lld-18") || command_works("ld.lld")
}

fn run(cmd: &mut Command) -> Result<()> {
  let out = cmd.output().with_context(|| format!("run {:?}", cmd))?;
  if out.status.success() {
    Ok(())
  } else {
    Err(anyhow!(
      "command failed (status {:?})\nstdout:\n{}\nstderr:\n{}",
      out.status.code(),
      String::from_utf8_lossy(&out.stdout),
      String::from_utf8_lossy(&out.stderr)
    ))
  }
}

fn assert_section_present_non_empty(bytes: &[u8], name: &str) -> Result<()> {
  let file = object::File::parse(bytes).context("parse object/elf")?;
  let sec = file
    .section_by_name(name)
    .ok_or_else(|| anyhow!("expected section {name} to exist"))?;
  if sec.size() == 0 {
    return Err(anyhow!("expected section {name} to be non-empty"));
  }
  Ok(())
}

fn assert_any_section_present_non_empty(bytes: &[u8], names: &[&str]) -> Result<()> {
  for name in names {
    if assert_section_present_non_empty(bytes, name).is_ok() {
      return Ok(());
    }
  }
  Err(anyhow!(
    "expected one of the following sections to exist and be non-empty: {names:?}"
  ))
}

fn assert_section_absent(bytes: &[u8], name: &str) -> Result<()> {
  let file = object::File::parse(bytes).context("parse object/elf")?;
  if file.section_by_name(name).is_some() {
    return Err(anyhow!("expected section {name} to be absent"));
  }
  Ok(())
}

fn assert_no_textrel_dynamic_tags(bytes: &[u8]) -> Result<()> {
  let file = object::File::parse(bytes).context("parse object/elf")?;
  let Some(dynamic) = file.section_by_name(".dynamic") else {
    // Static binaries have no dynamic section, so DT_TEXTREL can't apply.
    return Ok(());
  };
  let data = dynamic.data().context("read .dynamic section")?;

  // ELF64 little-endian: each entry is (i64 tag, u64 val).
  for ent in data.chunks_exact(16) {
    let tag = i64::from_le_bytes(ent[0..8].try_into().unwrap());
    let val = u64::from_le_bytes(ent[8..16].try_into().unwrap());
    if tag == 0 {
      break; // DT_NULL
    }
    // DT_TEXTREL (22) indicates text relocations are present.
    if tag == 22 {
      return Err(anyhow!("unexpected DT_TEXTREL in PIE executable"));
    }
    // DT_FLAGS (30) can include DF_TEXTREL (0x4).
    if tag == 30 && (val & 0x4) != 0 {
      return Err(anyhow!("unexpected DF_TEXTREL in DT_FLAGS for PIE executable"));
    }
  }
  Ok(())
}

fn build_statepoint_object() -> Result<Vec<u8>> {
  native_js::llvm::init_native_target().expect("failed to initialize native LLVM target");

  // Build a small statepoint/stackmap PoC module:
  // - `main` calls `test`.
  // - `test` is `gc \"coreclr\"` and keeps a GC pointer live across a call, which forces
  //   statepoint rewriting and `.llvm_stackmaps` emission.
  let context = Context::create();
  let module = context.create_module("stackmaps_link");
  let builder = context.create_builder();

  let gc_ptr = gc::gc_ptr_type(&context);
  let i8_ty = context.i8_type();
  let i32_ty = context.i32_type();
  let i64_ty = context.i64_type();

  let callee_ty = context.void_type().fn_type(&[], false);
  let callee = module.add_function("callee", callee_ty, None);
  let callee_entry = context.append_basic_block(callee, "entry");
  builder.position_at_end(callee_entry);
  builder.build_return(None).unwrap();

  let test_ty = gc_ptr.fn_type(&[gc_ptr.into()], false);
  let test_fn = module.add_function("test", test_ty, None);
  gc::set_default_gc_strategy(&test_fn).expect("GC strategy contains NUL byte");

  let test_entry = context.append_basic_block(test_fn, "entry");
  builder.position_at_end(test_entry);
  builder.build_call(callee, &[], "call_callee").unwrap();
  let arg0 = test_fn
    .get_first_param()
    .expect("missing arg0")
    .into_pointer_value();
  builder.build_return(Some(&arg0)).unwrap();

  // Linker-script defined symbols exported by `runtime-native/link/stackmaps*.ld`.
  let start_sym = module.add_global(i8_ty, None, native_js::link::LLVM_STACKMAPS_START_SYM);
  let stop_sym = module.add_global(i8_ty, None, native_js::link::LLVM_STACKMAPS_STOP_SYM);

  let main_ty = i32_ty.fn_type(&[], false);
  let main_fn = module.add_function("main", main_ty, None);
  let main_entry = context.append_basic_block(main_fn, "entry");
  builder.position_at_end(main_entry);
  builder
    .build_call(test_fn, &[gc_ptr.const_null().into()], "call_test")
    .unwrap();

  // Ensure the linked output actually retained `.llvm_stackmaps` (and that the linker script
  // symbols delimit a non-empty range).
  let start = builder
    .build_ptr_to_int(start_sym.as_pointer_value(), i64_ty, "stackmaps_start")
    .expect("ptrtoint start");
  let stop = builder
    .build_ptr_to_int(stop_sym.as_pointer_value(), i64_ty, "stackmaps_stop")
    .expect("ptrtoint stop");
  let diff = builder
    .build_int_sub(stop, start, "stackmaps_len")
    .expect("sub stop-start");
  let ok = builder
    .build_int_compare(IntPredicate::NE, diff, i64_ty.const_zero(), "stackmaps_present")
    .expect("icmp");
  let ret = builder
    .build_select(ok, i32_ty.const_zero(), i32_ty.const_int(1, false), "ret")
    .expect("select")
    .into_int_value();
  builder.build_return(Some(&ret)).unwrap();

  let mut target = emit::TargetConfig::default();
  target.cpu = "generic".to_string();
  target.features = "".to_string();
  target.opt_level = OptimizationLevel::None;
  target.reloc_mode = RelocMode::Default;
  target.code_model = CodeModel::Default;

  emit::emit_object_with_statepoints(&module, target).context("emit object with statepoints")
}
