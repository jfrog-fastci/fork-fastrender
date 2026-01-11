use object::{Object, ObjectSection};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
  // Ensure the build script reruns when the effective rustc flags change.
  println!("cargo:rerun-if-env-changed=CARGO_ENCODED_RUSTFLAGS");
  println!("cargo:rerun-if-env-changed=RUSTFLAGS");

  enforce_force_frame_pointers();
  build_rt_thread_tls();

  // Silence `unexpected_cfgs` warnings for cfgs set by this build script.
  println!("cargo::rustc-check-cfg=cfg(runtime_native_has_stackmap_test_artifact)");
  println!("cargo::rustc-check-cfg=cfg(runtime_native_no_stackmap_test_artifact)");

  // Integration tests `dlopen` small DSOs whose constructors call into this crate's
  // `rt_stackmaps_register` export. On ELF, executables only export symbols into
  // the dynamic symbol table when linked with `--export-dynamic` (aka `-rdynamic`).
  //
  // Keep this test-only so downstream consumers can opt into their preferred
  // export strategy (e.g. `-rdynamic` on the host executable or building
  // `runtime-native` as a shared library).
  let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
  if target_os == "linux" {
    println!("cargo:rustc-link-arg-tests=-Wl,--export-dynamic");
  }

  // Linker script (optional): expose `.llvm_stackmaps` as a loaded in-memory byte slice via
  // linker-defined start/stop symbols (see `link/stackmaps.ld`).
  println!("cargo:rerun-if-changed=link/stackmaps.ld");
  maybe_enable_stackmaps_linker_symbols();

  // Integration test artifact (x86_64 only): compile a tiny statepoint module and extract the
  // `Indirect [SP + off]` stackmap location so we can validate stack walking logic.
  maybe_build_stackmap_test_artifact();
}

fn build_rt_thread_tls() {
  println!("cargo:rerun-if-changed=src/rt_thread_tls.c");

  // Define `RT_THREAD` as a link-visible TLS symbol for native codegen.
  //
  // Rust's `#[thread_local]` static is still unstable, so we rely on a tiny C
  // translation unit compiled by the build script.
  cc::Build::new()
    .file("src/rt_thread_tls.c")
    .compile("runtime_native_tls");
}

fn enforce_force_frame_pointers() {
  // Escape hatch for experiments / debugging.
  if env::var_os("CARGO_FEATURE_ALLOW_OMIT_FRAME_POINTERS").is_some() {
    println!(
      "cargo:warning=runtime-native: building with `allow_omit_frame_pointers`; \
       the FP-based stack walker / GC root enumerator may crash or return incorrect results"
    );
    return;
  }

  let flags = rustflags();
  if force_frame_pointers_setting(&flags) == Some(true) {
    return;
  }

  let pretty_flags = if flags.is_empty() {
    "<none>".to_string()
  } else {
    flags.join(" ")
  };

  eprintln!(
    r#"runtime-native: missing required Rust frame pointer flag.

This crate contains an FP-based stack walker / GC root enumerator that assumes a stable frame-pointer (FP) chain.
Building without frame pointers can lead to stack walking failures and hard crashes.

Fix:
  - Set: RUSTFLAGS="-C force-frame-pointers=yes"
  - Or use the LLVM wrapper script (injects this flag automatically):
      # From the repository root:
      bash vendor/ecma-rs/scripts/cargo_llvm.sh build -p runtime-native
      # From the vendor/ecma-rs workspace root:
      bash scripts/cargo_llvm.sh build -p runtime-native

Escape hatch (unsafe; for experiments only):
  - Enable feature `allow_omit_frame_pointers`:
      bash vendor/ecma-rs/scripts/cargo_agent.sh build -p runtime-native --features allow_omit_frame_pointers

Detected rustflags:
  {pretty_flags}
"#
  );
  std::process::exit(1);
}

fn rustflags() -> Vec<String> {
  if let Ok(encoded) = env::var("CARGO_ENCODED_RUSTFLAGS") {
    // Cargo uses the ASCII unit separator to avoid quoting issues.
    let parts: Vec<String> = encoded
      .split('\u{1f}')
      .filter(|p| !p.is_empty())
      .map(|s| s.to_string())
      .collect();
    if !parts.is_empty() {
      return parts;
    }
  }

  if let Ok(raw) = env::var("RUSTFLAGS") {
    return raw
      .split_whitespace()
      .filter(|p| !p.is_empty())
      .map(|s| s.to_string())
      .collect();
  }

  Vec::new()
}

fn force_frame_pointers_setting(flags: &[String]) -> Option<bool> {
  // Accept both spellings:
  // - `-Cforce-frame-pointers=yes` / `-Cforce-frame-pointers=no`
  // - `-C force-frame-pointers=yes` / `-C force-frame-pointers=no`
  //
  // If the flag is specified multiple times, the *last* one wins (mirroring rustc behavior).
  let mut setting = None;
  let mut i = 0usize;
  while i < flags.len() {
    let flag = &flags[i];

    if flag == "-C" {
      if let Some(next) = flags.get(i + 1) {
        if let Some(v) = parse_force_frame_pointers_opt(next) {
          setting = Some(v);
        }
      }
      i += 2;
      continue;
    }

    if let Some(rest) = flag.strip_prefix("-C") {
      if let Some(v) = parse_force_frame_pointers_opt(rest) {
        setting = Some(v);
      }
    }

    i += 1;
  }
  setting
}

fn parse_force_frame_pointers_opt(opt: &str) -> Option<bool> {
  let opt = opt.trim();
  if !opt.starts_with("force-frame-pointers") {
    return None;
  }

  let rest = &opt["force-frame-pointers".len()..];
  if rest.is_empty() {
    // rustc expects an explicit value, but treat `-Cforce-frame-pointers` as "enabled" for
    // leniency (the build will still fail later if rustc rejects the flag).
    return Some(true);
  }

  let Some(value) = rest.strip_prefix('=') else {
    return None;
  };

  // rustc accepts several synonymous boolean values:
  // - true/yes/on
  // - false/no/off
  //
  // It also supports `non-leaf` / `always` behind `-Zunstable-options`. We treat `always` as
  // "enabled" and `non-leaf` as "disabled", since runtime-native requires a stable FP chain for
  // *all* frames.
  match value {
    "true" | "yes" | "on" | "always" => return Some(true),
    "false" | "no" | "off" | "non-leaf" => return Some(false),
    _ => {}
  }

  None
}

fn maybe_enable_stackmaps_linker_symbols() {
  let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
  if target_os != "linux" {
    return;
  }

  let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
  let script = manifest_dir.join("link").join("stackmaps.ld");

  // Pass an *absolute* path so the linker can always find it, regardless of the current working
  // directory Cargo uses for the link step.
  //
  // Note: We always inject this script for this crate's *tests* so stackmap-driven stack walking
  // integration tests can load `.llvm_stackmaps` even when the linker uses `--gc-sections`.
  //
  // Consumers can opt in to the same behavior for their final binaries with feature
  // `llvm_stackmaps_linker`.
  //
  // Important: avoid injecting the same script twice for test targets. When the feature is enabled,
  // `cargo:rustc-link-arg` applies to *all* targets (including tests), so also emitting
  // `cargo:rustc-link-arg-tests` would result in duplicate `-T stackmaps.ld` arguments.
  //
  // lld will accept the duplicate `-T`, but the linker script fragment uses `INSERT` and defines
  // `__start_llvm_stackmaps` / `__stop_llvm_stackmaps`. If the fragment runs twice, the second
  // invocation can override the symbols (often to an empty range after the first fragment consumed
  // the `.llvm_stackmaps` input sections), breaking runtime discovery.
  let feature_enabled = std::env::var_os("CARGO_FEATURE_LLVM_STACKMAPS_LINKER").is_some();

  // Prefer LLD when available: some linkers (notably `mold`) do not support the
  // GNU ld/LLD linker script features used by `stackmaps.ld` (SECTIONS/KEEP/INSERT).
  let lld_fuse = if Command::new("ld.lld-18")
    .arg("--version")
    .output()
    .map(|out| out.status.success())
    .unwrap_or(false)
  {
    Some("lld-18")
  } else if Command::new("ld.lld")
    .arg("--version")
    .output()
    .map(|out| out.status.success())
    .unwrap_or(false)
  {
    Some("lld")
  } else {
    None
  };
  if let Some(lld_fuse) = lld_fuse {
    if feature_enabled {
      println!("cargo:rustc-link-arg=-fuse-ld={lld_fuse}");
    } else {
      println!("cargo:rustc-link-arg-tests=-fuse-ld={lld_fuse}");
    }
  } else if feature_enabled || rustflags().iter().any(|f| f.contains("fuse-ld=mold")) {
    println!(
      "cargo:warning=runtime-native: ld.lld not found; stackmaps linker script requires GNU ld/LLD and may fail with mold"
    );
  }
  if feature_enabled {
    // Apply to all targets (including tests) when the consumer opts in to linker-defined symbols.
    println!("cargo:rustc-link-arg=-Wl,-T,{}", script.display());
  } else {
    // Otherwise, keep tests working without requiring the feature.
    println!("cargo:rustc-link-arg-tests=-Wl,-T,{}", script.display());
  }
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
  let objcopy = find_tool(&["llvm-objcopy-18", "llvm-objcopy"]);
  let (Some(opt), Some(llc), Some(objcopy)) = (opt, llc, objcopy) else {
    println!(
      "cargo:warning=runtime-native: LLVM tools not found (need opt/llc/llvm-objcopy); skipping stackmap integration artifact"
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

  if let Err(err) = rewrite_stackmap_sections_for_pie(&obj_path, &objcopy) {
    println!("cargo:warning=runtime-native: failed to rewrite stackmap sections: {err}");
    emit_stub();
    return;
  }

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
  // Note: we define `@safepoint` with **weak linkage** so integration tests can override the
  // symbol with an instrumented implementation (without causing duplicate-symbol link errors).
  let ir = r#"; ModuleID = 'runtime_native_stackmap_test'
source_filename = "runtime_native_stackmap_test"
target triple = "x86_64-unknown-linux-gnu"

; Keep the safepoint callee defined in this module so linking the generated
; object into Rust test binaries does not require any extra runtime stubs.
; Define `safepoint` as a *weak* symbol so tests can override it with a stronger
; definition (e.g. capturing FP/SP during stack walking validation) without
; causing duplicate symbol errors.
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
      // runtime-native requires statepoint GC roots to be spilled to addressable stack slots
      // (no stackmap `Register` locations).
      "--fixup-allow-gcptr-in-csr=false",
      "--fixup-max-csr-statepoints=0",
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
    .or_else(|| obj.section_by_name(".data.rel.ro.llvm_stackmaps"))
    .ok_or_else(|| "missing .llvm_stackmaps/.data.rel.ro.llvm_stackmaps section".to_string())?;
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

fn rewrite_stackmap_sections_for_pie(obj_path: &Path, objcopy: &str) -> Result<(), String> {
  let bytes = fs::read(obj_path).map_err(|e| format!("read {obj_path:?}: {e}"))?;
  let obj = object::File::parse(&*bytes).map_err(|e| format!("parse object: {e}"))?;

  let has_new_stackmaps = obj.section_by_name(".data.rel.ro.llvm_stackmaps").is_some();
  let has_old_stackmaps = obj.section_by_name(".llvm_stackmaps").is_some();
  if !has_new_stackmaps && has_old_stackmaps {
    let status = Command::new(objcopy)
      .args([
        "--rename-section",
        ".llvm_stackmaps=.data.rel.ro.llvm_stackmaps,alloc,load,data,contents",
      ])
      .arg(obj_path)
      .status()
      .map_err(|e| format!("spawn {objcopy}: {e}"))?;
    if !status.success() {
      return Err(format!("{objcopy} failed to rename .llvm_stackmaps"));
    }
  }

  let has_new_faultmaps = obj.section_by_name(".data.rel.ro.llvm_faultmaps").is_some();
  let has_old_faultmaps = obj.section_by_name(".llvm_faultmaps").is_some();
  if !has_new_faultmaps && has_old_faultmaps {
    let status = Command::new(objcopy)
      .args([
        "--rename-section",
        ".llvm_faultmaps=.data.rel.ro.llvm_faultmaps,alloc,load,data,contents",
      ])
      .arg(obj_path)
      .status()
      .map_err(|e| format!("spawn {objcopy}: {e}"))?;
    if !status.success() {
      return Err(format!("{objcopy} failed to rename .llvm_faultmaps"));
    }
  }

  Ok(())
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
