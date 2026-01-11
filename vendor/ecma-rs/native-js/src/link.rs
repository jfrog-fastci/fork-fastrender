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
//! PIE can be enabled via [`LinkOpts::pie`], but **safe** PIE support requires the runtime stackmap
//! reader to become relocation-aware (apply relocations / account for the load base) instead of
//! relying on text relocations.

use anyhow::Context;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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
  /// it directly) is incompatible with PIE unless the linker is allowed to emit text relocations.
  ///
  /// We therefore default to `pie: false` and use `-no-pie` unless the caller explicitly opts into
  /// PIE.
  pub pie: bool,
}

impl Default for LinkOpts {
  fn default() -> Self {
    Self {
      gc_sections: true,
      linker: LinkerFlavor::default(),
      pie: false,
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
    KEEP(*(.llvm_stackmaps .llvm_stackmaps.*))
    __fastr_stackmaps_end = .;
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
      .stdout(std::process::Stdio::null())
      .stderr(std::process::Stdio::null())
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
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .status()
    .is_ok()
  {
    Some(cand)
  } else {
    None
  }
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

  let mut cmd = Command::new(clang);
  cmd.arg("-o").arg(output_path);

  match opts.linker {
    LinkerFlavor::System => {}
    LinkerFlavor::Lld => {
      cmd.arg("-fuse-ld=lld");
    }
  }

  if opts.pie {
    if matches!(opts.linker, LinkerFlavor::Lld) {
      // Allow text relocations for experimentation. Safe PIE support requires relocation-aware
      // stackmap parsing; see module docs.
      cmd.arg("-Wl,-z,notext");
    }
  } else {
    cmd.arg("-no-pie");
  }

  if opts.gc_sections {
    cmd.arg("-Wl,--gc-sections");
  }

  cmd.arg(format!("-Wl,-T,{}", script_path.display()));

  for obj in object_files {
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

  if opts.pie {
    if matches!(opts.linker, LinkerFlavor::Lld) {
      cmd.arg("-Wl,-z,notext");
    }
  } else {
    cmd.arg("-no-pie");
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

  if opts.pie {
    if matches!(opts.linker, LinkerFlavor::Lld) {
      cmd.arg("-Wl,-z,notext");
    }
  } else {
    cmd.arg("-no-pie");
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
