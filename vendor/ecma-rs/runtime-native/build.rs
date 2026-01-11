use object::{Object, ObjectSection};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
  // Silence `unexpected_cfgs` warnings for cfgs set by this build script.
  println!("cargo::rustc-check-cfg=cfg(runtime_native_has_stackmap_test_artifact)");
  println!("cargo::rustc-check-cfg=cfg(runtime_native_no_stackmap_test_artifact)");

  // Linker script (optional): expose `.llvm_stackmaps` as a loaded in-memory byte slice via
  // linker-defined start/stop symbols (see `link/stackmaps.ld`).
  println!("cargo:rerun-if-changed=link/stackmaps.ld");
  maybe_enable_stackmaps_linker_symbols();

  // Integration test artifact (x86_64 only): compile a tiny statepoint module and extract the
  // `Indirect [SP + off]` stackmap location so we can validate stack walking logic.
  maybe_build_stackmap_test_artifact();
}

fn maybe_enable_stackmaps_linker_symbols() {
  // Only inject the linker script when the consumer opted in to linker-defined stackmap symbols.
  // This keeps runtime-native usable as a general-purpose library (tests, tools, C linking)
  // without requiring a custom linker script.
  if std::env::var_os("CARGO_FEATURE_LLVM_STACKMAPS_LINKER").is_none() {
    return;
  }
  let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
  if target_os != "linux" {
    return;
  }

  let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
  let script = manifest_dir.join("link").join("stackmaps.ld");

  // Allow `.llvm_stackmaps` relocations in PIE binaries when using lld.
  //
  // LLVM 18 emits absolute relocations in `.llvm_stackmaps` that can otherwise
  // trigger "TEXTREL" style link failures under `-pie`. See `docs/runtime-native.md`.
  println!("cargo:rustc-link-arg=-Wl,-z,notext");

  // Pass an *absolute* path so the linker can always find it, regardless of the current working
  // directory Cargo uses for the link step.
  println!("cargo:rustc-link-arg=-Wl,-T,{}", script.display());
}

fn maybe_build_stackmap_test_artifact() {
  let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
  let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();

  if target_os != "linux" || target_arch != "x86_64" {
    emit_stub();
    return;
  }

  let opt = find_tool(&["opt-18", "opt"]);
  let llc = find_tool(&["llc-18", "llc"]);
  let (Some(opt), Some(llc)) = (opt, llc) else {
    println!(
      "cargo:warning=runtime-native: LLVM tools not found (need opt/llc); skipping stackmap integration artifact"
    );
    emit_stub();
    return;
  };

  let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR not set"));

  let input_ll = out_dir.join("stackmap_test_input.ll");
  let rewritten_ll = out_dir.join("stackmap_test_rewritten.ll");
  let obj_path = out_dir.join("stackmap_test.o");
  let data_rs = out_dir.join("stackmap_test_data.rs");

  if let Err(err) = generate_llvm_module(&input_ll, &rewritten_ll, &obj_path, &opt, &llc) {
    println!("cargo:warning=runtime-native: failed to build stackmap test artifact: {err}");
    emit_stub();
    return;
  }

  let (inst_offset, sp_offset) = match parse_stackmap_offsets(&obj_path) {
    Ok(v) => v,
    Err(err) => {
      println!("cargo:warning=runtime-native: failed to parse .llvm_stackmaps: {err}");
      emit_stub();
      return;
    }
  };

  fs::write(
    &data_rs,
    format!(
      "pub const STACKMAP_INSTRUCTION_OFFSET: u32 = {inst_offset};\n\
       pub const STACKMAP_SP_OFFSET: i32 = {sp_offset};\n"
    ),
  )
  .expect("write stackmap_test_data.rs");

  // Make the constants available to integration tests.
  println!(
    "cargo:rustc-env=RUNTIME_NATIVE_STACKMAP_TEST_DATA_RS={}",
    data_rs.display()
  );

  // Link the generated object only into this crate's *tests*.
  //
  // The `.llvm_stackmaps` section contains relocations against code addresses and is not always
  // PIC-safe for shared-library builds. We only need this artifact for the integration tests that
  // validate stack walking logic.
  println!("cargo:rustc-link-arg-tests={}", obj_path.display());

  // Enable the integration test.
  println!("cargo:rustc-cfg=runtime_native_has_stackmap_test_artifact");
}

fn emit_stub() {
  // Integration test will be compiled out.
  println!("cargo:rustc-cfg=runtime_native_no_stackmap_test_artifact");
}

fn find_tool(candidates: &[&str]) -> Option<String> {
  for &name in candidates {
    let ok = Command::new(name)
      .arg("--version")
      .output()
      .map(|out| out.status.success())
      .unwrap_or(false);
    if ok {
      return Some(name.to_string());
    }
  }
  None
}

fn generate_llvm_module(
  input_ll: &Path,
  rewritten_ll: &Path,
  obj_path: &Path,
  opt: &str,
  llc: &str,
) -> Result<(), String> {
  // A tiny module that:
  //   - takes a GC pointer argument
  //   - makes a call to `@safepoint`
  //   - keeps the pointer live across the call
  //
  // `opt -passes=rewrite-statepoints-for-gc` rewrites the call into a statepoint and produces a
  // `.llvm_stackmaps` record keyed by the return address (the instruction after the call).
  //
  // Note: we define `@safepoint` with **weak linkage** so integration tests can override the symbol
  // with an instrumented implementation (without causing duplicate-symbol link errors).
  let ir = r#"; ModuleID = 'runtime_native_stackmap_test'
source_filename = "runtime_native_stackmap_test"
target triple = "x86_64-unknown-linux-gnu"

; Define `safepoint` as a *weak* symbol so tests can override it with a stronger
; definition (used to capture fp/sp during stack walking validation) without
; causing a duplicate-symbol link error.
define weak void @safepoint() noinline nounwind {
entry:
  ret void
}

 define ptr addrspace(1) @test_fn(ptr addrspace(1) %p) gc "coreclr" {
 entry:
   call void @safepoint()
  ret ptr addrspace(1) %p
}
"#;

  fs::write(input_ll, ir).map_err(|e| format!("write {input_ll:?}: {e}"))?;

  let opt_status = Command::new(opt)
    .args(["-passes=rewrite-statepoints-for-gc", "-S"])
    .arg(input_ll)
    .arg("-o")
    .arg(rewritten_ll)
    .status()
    .map_err(|e| format!("spawn {opt}: {e}"))?;
  if !opt_status.success() {
    return Err(format!("{opt} failed"));
  }

  let llc_status = Command::new(llc)
    .args([
      "-O0",
      "-filetype=obj",
      "-frame-pointer=all",
      "-relocation-model=pic",
    ])
    .arg(rewritten_ll)
    .arg("-o")
    .arg(obj_path)
    .status()
    .map_err(|e| format!("spawn {llc}: {e}"))?;
  if !llc_status.success() {
    return Err(format!("{llc} failed"));
  }

  Ok(())
}

fn parse_stackmap_offsets(obj_path: &Path) -> Result<(u32, i32), String> {
  let bytes = fs::read(obj_path).map_err(|e| format!("read {obj_path:?}: {e}"))?;
  let obj = object::File::parse(&*bytes).map_err(|e| format!("parse object: {e}"))?;
  let section = obj
    .section_by_name(".llvm_stackmaps")
    .ok_or_else(|| "missing .llvm_stackmaps section".to_string())?;
  let data = section
    .data()
    .map_err(|e| format!("read .llvm_stackmaps data: {e}"))?;

  let mut cur = Cursor::new(data.as_ref());

  let version = cur.read_u8()?;
  if version != 3 {
    return Err(format!("unsupported stackmap version {version}"));
  }
  let _reserved1 = cur.read_u8()?;
  let _reserved2 = cur.read_u16()?;

  let num_functions = cur.read_u32()?;
  let num_constants = cur.read_u32()?;
  let num_records = cur.read_u32()?;

  // Skip function records. We only need per-record instruction offsets + locations for this test.
  for _ in 0..num_functions {
    cur.read_u64()?; // function address
    cur.read_u64()?; // stack size
    cur.read_u64()?; // record count
  }
  for _ in 0..num_constants {
    cur.read_u64()?;
  }

  if num_records != 1 {
    return Err(format!("expected 1 record, found {num_records}"));
  }

  let _patchpoint_id = cur.read_u64()?;
  let inst_offset = cur.read_u32()?;
  let _reserved = cur.read_u16()?;
  let num_locations = cur.read_u16()?;

  let mut sp_offset: Option<i32> = None;
  for _ in 0..num_locations {
    let loc_type = cur.read_u8()?;
    let _loc_reserved = cur.read_u8()?;
    let _loc_size = cur.read_u16()?;
    let dwarf_reg = cur.read_u16()?;
    let _loc_reserved2 = cur.read_u16()?;
    let offset_or_const = cur.read_i32()?;

    // Location type 3 = Indirect.
    // DWARF register 7 = RSP on x86_64 (SysV).
    if loc_type == 3 && dwarf_reg == 7 {
      sp_offset.get_or_insert(offset_or_const);
    }
  }

  // The live-out header is 8-byte aligned after the locations array:
  //   u16 Padding;
  //   u16 NumLiveOuts;
  cur.align_to(8)?;
  let _padding = cur.read_u16()?;
  let num_live_outs = cur.read_u16()?;
  for _ in 0..num_live_outs {
    cur.read_u16()?; // reg
    cur.read_u8()?; // reserved
    cur.read_u8()?; // size
  }
  cur.align_to(8)?;

  let sp_offset = sp_offset.ok_or_else(|| "no Indirect [SP + off] location found".to_string())?;
  Ok((inst_offset, sp_offset))
}

struct Cursor<'a> {
  bytes: &'a [u8],
  pos: usize,
}

impl<'a> Cursor<'a> {
  fn new(bytes: &'a [u8]) -> Self {
    Self { bytes, pos: 0 }
  }

  fn align_to(&mut self, align: usize) -> Result<(), String> {
    let rem = self.pos % align;
    if rem != 0 {
      self.pos = self
        .pos
        .checked_add(align - rem)
        .ok_or_else(|| "cursor overflow".to_string())?;
      if self.pos > self.bytes.len() {
        return Err("unexpected EOF".to_string());
      }
    }
    Ok(())
  }

  fn read_exact<const N: usize>(&mut self) -> Result<[u8; N], String> {
    let end = self
      .pos
      .checked_add(N)
      .ok_or_else(|| "cursor overflow".to_string())?;
    if end > self.bytes.len() {
      return Err("unexpected EOF".to_string());
    }
    let mut out = [0u8; N];
    out.copy_from_slice(&self.bytes[self.pos..end]);
    self.pos = end;
    Ok(out)
  }

  fn read_u8(&mut self) -> Result<u8, String> {
    Ok(self.read_exact::<1>()?[0])
  }

  fn read_u16(&mut self) -> Result<u16, String> {
    Ok(u16::from_le_bytes(self.read_exact::<2>()?))
  }

  fn read_u32(&mut self) -> Result<u32, String> {
    Ok(u32::from_le_bytes(self.read_exact::<4>()?))
  }

  fn read_u64(&mut self) -> Result<u64, String> {
    Ok(u64::from_le_bytes(self.read_exact::<8>()?))
  }

  fn read_i32(&mut self) -> Result<i32, String> {
    Ok(i32::from_le_bytes(self.read_exact::<4>()?))
  }
}
