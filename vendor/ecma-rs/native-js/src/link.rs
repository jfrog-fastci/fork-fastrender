//! Linking helpers for producing native executables that preserve LLVM stack maps.
//!
//! ## Why default to non-PIE on Linux?
//! LLVM's `.llvm_stackmaps` section (emitted by statepoints / `llvm.experimental.stackmap`) often
//! contains relocations against code addresses.
//!
//! When linking a PIE those relocations become dynamic relocations and can require the dynamic
//! loader to apply text relocations (`DT_TEXTREL`) if `.llvm_stackmaps` is placed in a read-only
//! segment. Many hardened toolchains reject this.
//!
//! `native-js` therefore links **non-PIE** (`-no-pie`) by default on Linux so stackmap relocations
//! are resolved at link time. This also keeps stackmap lookup simple: return addresses are stable
//! absolute addresses.
//!
//! ## Supporting PIE safely
//! PIE can be enabled via [`LinkOpts::pie`] **without** `DT_TEXTREL` by making the input
//! `.llvm_stackmaps` section writable before linking. This allows lld to emit normal dynamic
//! relocations against the writable section instead of requiring text relocations.
//!
//! The dynamic loader applies these relocations at startup, so the stackmap records contain the
//! final relocated absolute PCs at runtime, and stackmap lookup continues to work by comparing
//! return addresses directly.

use anyhow::Context;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::process::Stdio;

/// Symbol exported by the final ELF that points at the first byte of `.llvm_stackmaps`.
pub const FASTR_STACKMAPS_START_SYM: &str = "__fastr_stackmaps_start";
/// Symbol exported by the final ELF that points one byte past the end of `.llvm_stackmaps`.
pub const FASTR_STACKMAPS_END_SYM: &str = "__fastr_stackmaps_end";

#[derive(Clone, Copy, Debug, Default)]
pub enum LinkerFlavor {
  /// Use the system linker selected by `clang`.
  System,
  /// Use LLD via `clang -fuse-ld=lld`.
  #[default]
  Lld,
}

/// Options controlling how we link generated artifacts into an executable.
#[derive(Clone, Copy, Debug)]
pub struct LinkOpts {
  /// If true, pass `-Wl,--gc-sections` to the linker.
  ///
  /// Note: `.llvm_stackmaps` is still retained via our linker script fragment
  /// (`KEEP(*(.llvm_stackmaps ...))`).
  pub gc_sections: bool,
  pub linker: LinkerFlavor,
  /// Whether to produce a PIE executable.
  ///
  /// Ubuntu toolchains default to PIE, but LLVM stackmaps contain absolute relocations against
  /// function symbols. Preserving `.llvm_stackmaps` in a read-only segment (so the runtime can read
  /// it directly) is incompatible with PIE unless we either:
  /// - allow text relocations (`-Wl,-z,notext`), or
  /// - rewrite `.llvm_stackmaps` to be writable before linking so lld can relocate it normally.
  ///
  /// We therefore default to `pie: false` and use `-no-pie` unless the caller explicitly opts into
  /// PIE. On Linux, `pie: true` passes `-pie` explicitly so the link mode is reproducible across
  /// toolchains.
  pub pie: bool,

  /// Best-effort request for debug info during linking (`clang -g`).
  ///
  /// This does not generate debug info by itself; it only tells the linker driver to keep debug
  /// sections from the input objects (useful when those objects already contain DWARF).
  pub debug: bool,
}

impl Default for LinkOpts {
  fn default() -> Self {
    Self {
      gc_sections: true,
      linker: LinkerFlavor::default(),
      pie: false,
      debug: false,
    }
  }
}

/// Linker script fragment injected into the default linker script (via `INSERT AFTER`) so we
/// don't have to replace the entire default script.
///
/// We insert after `.text` (instead of the more intuitive `.rodata`) because lld does not create an
/// empty `.rodata` output section, and will error if an `INSERT AFTER .rodata` fragment is used
/// when the input objects don't contribute any `.rodata` (common in minimal binaries/tests).
///
/// `KEEP` ensures `.llvm_stackmaps` isn't discarded by `--gc-sections`.
const LLVM_STACKMAPS_LD_FRAGMENT: &str = r#"
SECTIONS {
  .llvm_stackmaps : {
    __fastr_stackmaps_start = .;
    __llvm_stackmaps_start = .;
    KEEP(*(.llvm_stackmaps .llvm_stackmaps.*))
    __fastr_stackmaps_end = .;
    __llvm_stackmaps_end = .;
  }
} INSERT AFTER .text;
"#;

fn write_stackmaps_linker_script(path: &Path) -> anyhow::Result<()> {
  fs::write(path, LLVM_STACKMAPS_LD_FRAGMENT).with_context(|| {
    format!(
      "failed to write linker script fragment to {}",
      path.display()
    )
  })?;
  Ok(())
}

fn exe_exists<P: AsRef<Path>>(path: P) -> bool {
  std::fs::metadata(path).is_ok()
}

fn find_clang() -> Option<&'static str> {
  // Prefer clang-18 (what our exec plan installs), but allow fallback for developer machines.
  for cand in ["clang-18", "clang"] {
    if Command::new(cand)
      .arg("--version")
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .status()
      .is_ok()
    {
      return Some(cand);
    }
  }
  None
}

fn find_clang_18() -> Option<&'static str> {
  let cand = "clang-18";
  if Command::new(cand)
    .arg("--version")
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status()
    .is_ok()
  {
    Some(cand)
  } else {
    None
  }
}

fn find_llvm_objcopy() -> Option<&'static str> {
  for cand in ["llvm-objcopy-18", "llvm-objcopy"] {
    if Command::new(cand)
      .arg("--version")
      .stdout(Stdio::null())
      .stderr(Stdio::null())
      .status()
      .is_ok()
    {
      return Some(cand);
    }
  }
  None
}

/// Link one or more object files into an ELF executable.
///
/// The resulting binary will export [`FASTR_STACKMAPS_START_SYM`] and [`FASTR_STACKMAPS_END_SYM`]
/// that delimit the `.llvm_stackmaps` section in memory.
pub fn link_elf_executable(output_path: &Path, object_files: &[PathBuf]) -> anyhow::Result<()> {
  link_elf_executable_with_options(output_path, object_files, LinkOpts::default())
}

pub fn link_elf_executable_with_options(
  output_path: &Path,
  object_files: &[PathBuf],
  opts: LinkOpts,
) -> anyhow::Result<()> {
  let clang = find_clang().context("unable to locate clang (expected `clang-18` or `clang`)")?;
  let out_dir = output_path
    .parent()
    .context("output_path must have a parent directory")?;

  fs::create_dir_all(out_dir)
    .with_context(|| format!("failed to create output directory {}", out_dir.display()))?;

  let script_path = out_dir.join("fastr_stackmaps.ld");
  write_stackmaps_linker_script(&script_path)?;

  // If producing a PIE, ensure `.llvm_stackmaps` is writable in the input objects so lld can apply
  // the required relocations without emitting DT_TEXTREL.
  //
  // We copy objects into a tempdir to avoid mutating the caller's build artifacts in-place.
  let mut patched_obj_dir: Option<tempfile::TempDir> = None;
  let mut object_files: Vec<PathBuf> = object_files.to_vec();
  if cfg!(target_os = "linux") && opts.pie {
    let objcopy = find_llvm_objcopy()
      .context("unable to locate llvm-objcopy (expected `llvm-objcopy-18` or `llvm-objcopy`)")?;
    let td = tempfile::tempdir().context("failed to create tempdir for stackmaps objcopy")?;
    let mut patched = Vec::with_capacity(object_files.len());
    for (idx, src) in object_files.iter().enumerate() {
      let dst = td.path().join(format!("obj{idx}.o"));
      fs::copy(src, &dst)
        .with_context(|| format!("failed to copy object {} to {}", src.display(), dst.display()))?;

      // `llvm-objcopy` is a no-op if the section doesn't exist, so we can apply this unconditionally.
      let status = Command::new(objcopy)
        .args([
          "--set-section-flags",
          ".llvm_stackmaps=alloc,load,contents,data",
        ])
        .arg(&dst)
        .status()
        .with_context(|| format!("failed to spawn {objcopy}"))?;
      if !status.success() {
        anyhow::bail!("{objcopy} failed with status {status}");
      }

      patched.push(dst);
    }
    object_files = patched;
    patched_obj_dir = Some(td);
  }
  // Keep the tempdir alive until after linking completes.
  let _patched_obj_dir = patched_obj_dir;

  let mut cmd = Command::new(clang);
  cmd.arg("-o").arg(output_path);

  if opts.debug {
    cmd.arg("-g");
  }

  match opts.linker {
    LinkerFlavor::System => {}
    LinkerFlavor::Lld => {
      cmd.arg("-fuse-ld=lld");
    }
  }

  if cfg!(target_os = "linux") {
    if opts.pie {
      cmd.arg("-pie");
    } else {
      cmd.arg("-no-pie");
    }
  }

  if opts.gc_sections {
    cmd.arg("-Wl,--gc-sections");
  }

  cmd.arg(format!("-Wl,-T,{}", script_path.display()));

  for obj in &object_files {
    cmd.arg(obj);
  }

  let status = cmd
    .status()
    .with_context(|| format!("failed to spawn {clang} for linking"))?;
  if !status.success() {
    anyhow::bail!("linker exited with status {status}");
  }

  if !exe_exists(output_path) {
    anyhow::bail!(
      "linker reported success but output file does not exist: {}",
      output_path.display()
    );
  }

  Ok(())
}

/// Link one or more **in-memory** object buffers into an ELF executable.
pub fn link_object_buffers_to_elf_executable(
  output_path: &Path,
  object_buffers: &[&[u8]],
  opts: LinkOpts,
) -> anyhow::Result<()> {
  let td = tempfile::tempdir().context("failed to create tempdir for object linking")?;
  let mut paths = Vec::with_capacity(object_buffers.len());
  for (idx, bytes) in object_buffers.iter().enumerate() {
    let path = td.path().join(format!("obj{idx}.o"));
    fs::write(&path, bytes)
      .with_context(|| format!("failed to write object to {}", path.display()))?;
    paths.push(path);
  }

  link_elf_executable_with_options(output_path, &paths, opts)
}

/// Link an in-memory LLVM bitcode module into an ELF executable and return the resulting bytes.
///
/// This is primarily used for testing `clang -flto` behavior (LTO + section GC) without having to
/// write intermediate artifacts into the repository.
#[cfg(target_os = "linux")]
pub fn link_bitcode_to_exe(bitcode: &[u8], opts: LinkOpts) -> anyhow::Result<Vec<u8>> {
  let clang =
    find_clang_18().context("unable to locate clang-18 (required for LLVM 18 LTO bitcode)")?;

  let td = tempfile::tempdir().context("failed to create tempdir for LTO link")?;
  let bc_path = td.path().join("module.bc");
  let exe_path = td.path().join("a.out");
  let script_path = td.path().join("fastr_stackmaps.ld");

  fs::write(&bc_path, bitcode)
    .with_context(|| format!("failed to write bitcode to {}", bc_path.display()))?;
  write_stackmaps_linker_script(&script_path)?;

  let mut cmd = Command::new(clang);
  cmd.arg("-flto");

  if opts.debug {
    cmd.arg("-g");
  }

  if cfg!(target_os = "linux") {
    if opts.pie {
      cmd.arg("-pie");
      if matches!(opts.linker, LinkerFlavor::Lld) {
        cmd.arg("-Wl,-z,notext");
      }
    } else {
      cmd.arg("-no-pie");
    }
  }

  match opts.linker {
    LinkerFlavor::System => {}
    LinkerFlavor::Lld => {
      cmd.arg("-fuse-ld=lld");
    }
  }

  if opts.gc_sections {
    cmd.arg("-Wl,--gc-sections");
  }

  cmd.arg(format!("-Wl,-T,{}", script_path.display()));
  cmd.arg("-o").arg(&exe_path);
  cmd.arg(&bc_path);

  let out = cmd
    .output()
    .with_context(|| format!("failed to spawn {clang} for LTO linking"))?;
  if !out.status.success() {
    anyhow::bail!(
      "{clang} -flto failed with status {status}\nstdout:\n{stdout}\nstderr:\n{stderr}",
      status = out.status,
      stdout = String::from_utf8_lossy(&out.stdout),
      stderr = String::from_utf8_lossy(&out.stderr),
    );
  }

  if !exe_exists(&exe_path) {
    anyhow::bail!(
      "linker reported success but output file does not exist: {}",
      exe_path.display()
    );
  }

  fs::read(&exe_path).with_context(|| format!("failed to read {}", exe_path.display()))
}

#[cfg(not(target_os = "linux"))]
pub fn link_bitcode_to_exe(_bitcode: &[u8], _opts: LinkOpts) -> anyhow::Result<Vec<u8>> {
  anyhow::bail!("link_bitcode_to_exe is only supported on Linux for now")
}

/// Link one or more LLVM bitcode modules (`.bc`) into an ELF executable using LTO.
///
/// This is useful for testing the behavior of LLVM's stackmap emission when multiple bitcode
/// modules are linked under `-flto` (LLVM tends to emit a single merged StackMaps blob in this
/// mode, rather than concatenated per-object blobs).
///
/// The resulting binary will export [`FASTR_STACKMAPS_START_SYM`] and [`FASTR_STACKMAPS_END_SYM`]
/// that delimit the `.llvm_stackmaps` section in memory.
pub fn link_elf_executable_lto(output_path: &Path, bitcode_files: &[PathBuf]) -> anyhow::Result<()> {
  let opts = LinkOpts::default();
  let clang = find_clang_18().context("unable to locate clang-18 (required for LLVM 18 LTO bitcode)")?;
  let out_dir = output_path
    .parent()
    .context("output_path must have a parent directory")?;

  fs::create_dir_all(out_dir)
    .with_context(|| format!("failed to create output directory {}", out_dir.display()))?;

  let script_path = out_dir.join("fastr_stackmaps.ld");
  write_stackmaps_linker_script(&script_path)?;

  let mut cmd = Command::new(clang);
  cmd.arg("-flto=full");

  if cfg!(target_os = "linux") {
    if opts.pie {
      cmd.arg("-pie");
      if matches!(opts.linker, LinkerFlavor::Lld) {
        cmd.arg("-Wl,-z,notext");
      }
    } else {
      cmd.arg("-no-pie");
    }
  }

  match opts.linker {
    LinkerFlavor::System => {}
    LinkerFlavor::Lld => {
      cmd.arg("-fuse-ld=lld");
    }
  }

  if opts.gc_sections {
    cmd.arg("-Wl,--gc-sections");
  }

  cmd.arg(format!("-Wl,-T,{}", script_path.display()));
  cmd.arg("-o").arg(output_path);

  for bc in bitcode_files {
    cmd.arg(bc);
  }

  let out = cmd
    .output()
    .with_context(|| format!("failed to spawn {clang} for LTO linking"))?;
  if !out.status.success() {
    anyhow::bail!(
      "{clang} -flto failed with status {status}\nstdout:\n{stdout}\nstderr:\n{stderr}",
      status = out.status,
      stdout = String::from_utf8_lossy(&out.stdout),
      stderr = String::from_utf8_lossy(&out.stderr),
    );
  }

  if !exe_exists(output_path) {
    anyhow::bail!(
      "linker reported success but output file does not exist: {}",
      output_path.display()
    );
  }

  Ok(())
}

/// Minimal system linker wrapper used by the early AOT pipeline.
///
/// This intentionally does *not* depend on the stackmap linker script fragment above; it is meant
/// for small libc-only executables (e.g. `puts`-based smoke tests).
pub fn link_object_to_executable(obj_path: &Path, exe_path: &Path) -> Result<(), crate::NativeJsError> {
  if !cfg!(target_os = "linux") {
    return Err(crate::NativeJsError::UnsupportedPlatform {
      target_os: std::env::consts::OS.to_string(),
    });
  }

  let clang = find_program(&["clang-18", "clang"]).ok_or(crate::NativeJsError::ToolNotFound(
    "clang-18/clang",
  ))?;

  let have_lld = find_program(&["ld.lld", "ld.lld-18", "lld-18", "lld"]).is_some();

  let mut cmd = Command::new(&clang);
  cmd.arg("-O2").arg("-Wl,--gc-sections").arg("-no-pie");
  if have_lld {
    cmd.arg("-fuse-ld=lld");
  }
  cmd.arg(obj_path).arg("-o").arg(exe_path);

  let cmd_dbg = format!("{cmd:?}");
  let output = cmd.output().map_err(crate::NativeJsError::LinkerSpawnFailed)?;
  if !output.status.success() {
    return Err(crate::NativeJsError::LinkerFailed {
      cmd: cmd_dbg,
      stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    });
  }

  Ok(())
}

fn find_program(names: &[&str]) -> Option<PathBuf> {
  let path = std::env::var_os("PATH")?;
  for dir in std::env::split_paths(&path) {
    for name in names {
      let candidate = dir.join(name);
      if candidate.is_file() {
        return Some(candidate);
      }
    }
  }
  None
}
