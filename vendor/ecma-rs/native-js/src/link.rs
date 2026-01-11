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
//! PIE can be enabled via [`LinkOpts::pie`] **without** `DT_TEXTREL` by relocating LLVM stackmaps
//! (and faultmaps, if present) into a RELRO-friendly data section before the final link.
//!
//! Concretely, we rewrite input objects to rename:
//!
//! - `.llvm_stackmaps` → `.data.rel.ro.llvm_stackmaps`
//! - `.llvm_faultmaps` → `.data.rel.ro.llvm_faultmaps`
//!
//! using `llvm-objcopy --rename-section ...`. This ensures any required relocations are applied to
//! RW memory (as normal dynamic relocations) and avoids text relocations.
//!
//! The dynamic loader applies these relocations at startup, so the stackmap records contain the
//! final relocated absolute PCs at runtime, and stackmap lookup continues to work by comparing
//! return addresses directly.
//!
//! Note: the `clang -flto` helpers in this module currently only support **non-PIE** output. LTO
//! emits `.llvm_stackmaps` during link-time codegen, so we can't pre-patch input objects with
//! `llvm-objcopy` the way we do for object-file linking.

use anyhow::Context;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::process::Stdio;

/// Symbol exported by the final ELF that points at the first byte of stackmaps.
pub const LLVM_STACKMAPS_START_SYM: &str = "__start_llvm_stackmaps";
/// Symbol exported by the final ELF that points one byte past the end of stackmaps.
pub const LLVM_STACKMAPS_STOP_SYM: &str = "__stop_llvm_stackmaps";

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
  /// Note: stackmaps are still retained via our linker script fragment (`KEEP(*(...))`).
  pub gc_sections: bool,
  pub linker: LinkerFlavor,
  /// Whether to produce a PIE executable.
  ///
  /// Ubuntu toolchains default to PIE, but LLVM stackmaps contain absolute relocations against
  /// function symbols.
  ///
  /// When producing PIE, we avoid `DT_TEXTREL` by rewriting input objects to rename
  /// `.llvm_stackmaps` → `.data.rel.ro.llvm_stackmaps` (and `.llvm_faultmaps` →
  /// `.data.rel.ro.llvm_faultmaps`) before linking. This allows the dynamic
  /// loader to apply relocations to RW memory and then protect it via RELRO.
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

/// Linker script fragment injected into the default linker script (via the GNU ld/LLD `INSERT`
/// mechanism) so we don't have to replace the entire default script.
///
/// We use different fragments depending on the link mode:
/// - non-PIE: `stackmaps_nopie.ld`, anchored after `.text` (always present) and emitting a
///   dedicated `.llvm_stackmaps` output section.
/// - PIE (lld): `stackmaps.ld`, anchored before `.dynamic` to keep stackmaps in RELRO-friendly data
///   without tripping lld's RELRO contiguity checks.
/// - PIE (GNU ld): `stackmaps_gnuld.ld`, to avoid producing an RWX LOAD segment when placing
///   writable stackmaps/faultmaps.
///
/// Keep this in sync with `runtime-native/link/stackmaps*.ld`.
const LLVM_STACKMAPS_LD_FRAGMENT: &str = include_str!("../../runtime-native/link/stackmaps.ld");
const LLVM_STACKMAPS_LD_NOPIE_FRAGMENT: &str =
  include_str!("../../runtime-native/link/stackmaps_nopie.ld");
const LLVM_STACKMAPS_LD_GNULD_FRAGMENT: &str =
  include_str!("../../runtime-native/link/stackmaps_gnuld.ld");

fn stackmaps_linker_script_fragment(opts: LinkOpts) -> &'static str {
  if cfg!(target_os = "linux") && !opts.pie {
    return LLVM_STACKMAPS_LD_NOPIE_FRAGMENT;
  }
  // GNU ld + PIE: stackmaps often need to be writable for dynamic relocations.
  // Inserting a writable `.data.rel.ro.*` section immediately after `.text` can
  // result in an RWX segment on GNU ld. Prefer a `.dynamic`-anchored fragment in
  // that configuration.
  if cfg!(target_os = "linux") && opts.pie && matches!(opts.linker, LinkerFlavor::System) {
    LLVM_STACKMAPS_LD_GNULD_FRAGMENT
  } else {
    LLVM_STACKMAPS_LD_FRAGMENT
  }
}

fn write_stackmaps_linker_script(path: &Path, opts: LinkOpts) -> anyhow::Result<()> {
  fs::write(path, stackmaps_linker_script_fragment(opts)).with_context(|| {
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
      .is_ok_and(|s| s.success())
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
    .is_ok_and(|s| s.success())
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
      .is_ok_and(|s| s.success())
    {
      return Some(cand);
    }
  }
  None
}

/// Link one or more object files into an ELF executable.
///
/// The resulting binary will export [`LLVM_STACKMAPS_START_SYM`] and [`LLVM_STACKMAPS_STOP_SYM`]
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
  // `Path::parent` of a relative filename like `out` is `Some("")`. Treat that as the current
  // directory so `create_dir_all` and script emission work as expected.
  let out_dir: &Path = if out_dir.as_os_str().is_empty() {
    Path::new(".")
  } else {
    out_dir
  };

  fs::create_dir_all(out_dir)
    .with_context(|| format!("failed to create output directory {}", out_dir.display()))?;

  // Use a per-invocation temp file for the linker script fragment to avoid:
  // - polluting the output directory with build artifacts
  // - collisions when multiple linkers run concurrently and happen to share an output directory
  //   (e.g. temp file outputs in `/tmp`).
  let script_dir = tempfile::tempdir().context("failed to create tempdir for stackmaps linker script")?;
  let script_path = script_dir.path().join("llvm_stackmaps.ld");
  write_stackmaps_linker_script(&script_path, opts)?;

  // If producing a PIE, relocate `.llvm_stackmaps` / `.llvm_faultmaps` into
  // `.data.rel.ro.llvm_stackmaps` / `.data.rel.ro.llvm_faultmaps` in the input objects so lld can
  // apply the required relocations without emitting DT_TEXTREL.
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
      fs::copy(src, &dst).with_context(|| {
        format!(
          "failed to copy object {} to {}",
          src.display(),
          dst.display()
        )
      })?;

      // `llvm-objcopy` is a no-op if the section doesn't exist, so we can apply this
      // unconditionally.
      let status = Command::new(objcopy)
        .args([
          "--rename-section",
          ".llvm_stackmaps=.data.rel.ro.llvm_stackmaps,alloc,load,data,contents",
          "--rename-section",
          ".llvm_faultmaps=.data.rel.ro.llvm_faultmaps,alloc,load,data,contents",
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
      cmd.arg(format!("-fuse-ld={}", lld_fuse_arg()));
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
  let script_path = td.path().join("llvm_stackmaps.ld");

  fs::write(&bc_path, bitcode)
    .with_context(|| format!("failed to write bitcode to {}", bc_path.display()))?;
  write_stackmaps_linker_script(&script_path, opts)?;

  let mut cmd = Command::new(clang);
  cmd.arg("-flto");
  // Clang performs codegen in a separate process, so it won't see our in-process
  // `LLVMParseCommandLineOptions` configuration. Pass the equivalent backend
  // flag to ensure statepoint GC roots are spilled to stack slots (never
  // stackmap `Register` locations).
  cmd
    .arg("-mllvm")
    .arg("--fixup-allow-gcptr-in-csr=false")
    .arg("-mllvm")
    .arg("--fixup-max-csr-statepoints=0");

  if opts.debug {
    cmd.arg("-g");
  }

  if cfg!(target_os = "linux") {
    if opts.pie {
      anyhow::bail!(
        "PIE is not supported for `clang -flto` helpers without DT_TEXTREL; \
use object-file linking with `LinkOpts {{ pie: true, .. }}` (objcopy-patched), or keep `pie: false`."
      );
    }
    cmd.arg("-no-pie");
  }

  match opts.linker {
    LinkerFlavor::System => {}
    LinkerFlavor::Lld => {
      cmd.arg(format!("-fuse-ld={}", lld_fuse_arg()));
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
/// The resulting binary will export [`LLVM_STACKMAPS_START_SYM`] and [`LLVM_STACKMAPS_STOP_SYM`]
/// that delimit the `.llvm_stackmaps` section in memory.
pub fn link_elf_executable_lto(
  output_path: &Path,
  bitcode_files: &[PathBuf],
) -> anyhow::Result<()> {
  let opts = LinkOpts::default();
  let clang =
    find_clang_18().context("unable to locate clang-18 (required for LLVM 18 LTO bitcode)")?;
  let out_dir = output_path
    .parent()
    .context("output_path must have a parent directory")?;
  let out_dir: &Path = if out_dir.as_os_str().is_empty() {
    Path::new(".")
  } else {
    out_dir
  };

  fs::create_dir_all(out_dir)
    .with_context(|| format!("failed to create output directory {}", out_dir.display()))?;

  let script_path = out_dir.join("llvm_stackmaps.ld");
  write_stackmaps_linker_script(&script_path, opts)?;

  let mut cmd = Command::new(clang);
  cmd.arg("-flto=full");
  // See `link_bitcode_to_exe`: ensure statepoint roots are forced into stack
  // slots during link-time codegen.
  cmd
    .arg("-mllvm")
    .arg("--fixup-allow-gcptr-in-csr=false")
    .arg("-mllvm")
    .arg("--fixup-max-csr-statepoints=0");

  if cfg!(target_os = "linux") {
    // We don't currently support PIE for this LTO helper (see module docs).
    cmd.arg("-no-pie");
  }

  match opts.linker {
    LinkerFlavor::System => {}
    LinkerFlavor::Lld => {
      cmd.arg(format!("-fuse-ld={}", lld_fuse_arg()));
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
/// This is used by the "emit executable" helper for quick smoke tests / debugging. It still wires
/// in the stackmaps linker fragment so `--gc-sections` doesn't discard `.llvm_stackmaps` if the
/// generated object happens to include them.
pub fn link_object_to_executable(
  obj_path: &Path,
  exe_path: &Path,
) -> Result<(), crate::NativeJsError> {
  if !cfg!(target_os = "linux") {
    return Err(crate::NativeJsError::UnsupportedPlatform {
      target_os: std::env::consts::OS.to_string(),
    });
  }

  let clang = find_program(&["clang-18", "clang"])
    .ok_or(crate::NativeJsError::ToolNotFound("clang-18/clang"))?;

  let have_lld = find_program(&["ld.lld", "ld.lld-18", "lld-18", "lld"]).is_some();

  // Always inject the stackmaps linker script fragment:
  // - defines `__fastr_stackmaps_start/end` (and `__llvm_*` aliases)
  // - `KEEP`s `.llvm_stackmaps` so `--gc-sections` can't discard it.
  let td = tempfile::tempdir().map_err(crate::NativeJsError::TempDirCreateFailed)?;
  let script_path = td.path().join("fastr_stackmaps.ld");
  fs::write(
    &script_path,
    stackmaps_linker_script_fragment(LinkOpts::default()),
  )
  .map_err(|source| {
    crate::NativeJsError::Io {
      path: script_path.clone(),
      source,
    }
  })?;

  let mut cmd = Command::new(&clang);
  cmd
    .arg("-O2")
    .arg("-Wl,--gc-sections")
    .arg(format!("-Wl,-T,{}", script_path.display()))
    .arg("-no-pie");
  if have_lld {
    cmd.arg(format!("-fuse-ld={}", lld_fuse_arg()));
  }
  cmd.arg(obj_path).arg("-o").arg(exe_path);

  let cmd_dbg = format!("{cmd:?}");
  let output = cmd
    .output()
    .map_err(crate::NativeJsError::LinkerSpawnFailed)?;
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

fn lld_fuse_arg() -> &'static str {
  if find_program(&["ld.lld-18"]).is_some() {
    "lld-18"
  } else {
    "lld"
  }
}
