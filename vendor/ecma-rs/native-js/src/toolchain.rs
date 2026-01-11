use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{anyhow, bail, Context, Result};

#[derive(Clone, Copy, Debug)]
pub enum OptLevel {
  O0,
  O1,
  O2,
  O3,
}

impl OptLevel {
  fn clang_flag(self) -> &'static str {
    match self {
      Self::O0 => "-O0",
      Self::O1 => "-O1",
      Self::O2 => "-O2",
      Self::O3 => "-O3",
    }
  }
}

#[derive(Clone, Debug)]
pub struct LlvmToolchain {
  pub clang: PathBuf,
  pub llvm_objdump: PathBuf,
}

impl LlvmToolchain {
  pub fn detect() -> Result<Self> {
    let clang = find_executable(&["clang-18", "clang"])
      .ok_or_else(|| anyhow!("failed to find clang (expected `clang-18` in PATH)"))?;
    let llvm_objdump = find_executable(&["llvm-objdump-18", "llvm-objdump"])
      .ok_or_else(|| anyhow!("failed to find llvm-objdump (expected `llvm-objdump-18` in PATH)"))?;
    Ok(Self { clang, llvm_objdump })
  }

  pub fn host_target_triple(&self) -> Result<String> {
    let out = Command::new(&self.clang)
      .arg("-dumpmachine")
      .output()
      .with_context(|| format!("failed to run clang at {}", self.clang.display()))?;
    if !out.status.success() {
      bail!(
        "clang -dumpmachine failed (status={})\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
      );
    }
    let triple = String::from_utf8(out.stdout)?.trim().to_owned();
    if triple.is_empty() {
      bail!("clang -dumpmachine returned empty target triple");
    }
    Ok(triple)
  }

  pub fn compile_ll_to_object(
    &self,
    ll_path: &Path,
    object_path: &Path,
    opt_level: OptLevel,
  ) -> Result<()> {
    let out = Command::new(&self.clang)
      .arg(opt_level.clang_flag())
      // LLVM statepoint stackmaps can legally report gc-live roots as `Register` locations (often
      // callee-saved registers). That is only safe if the GC entry stub captures the full register
      // file before running any code that may clobber those registers.
      //
      // Our runtime-native helpers are normal Rust functions; their prologue may save/repurpose
      // callee-saved registers before invoking the collector. Force LLVM to materialize GC roots in
      // caller-owned stack slots by disabling CSR-based root passing in the backend.
      .arg("-mllvm")
      .arg("--fixup-allow-gcptr-in-csr=false")
      .arg("-mllvm")
      .arg("--fixup-max-csr-statepoints=0")
      .arg("-c")
      .arg(ll_path)
      .arg("-o")
      .arg(object_path)
      .output()
      .with_context(|| format!("failed to run clang at {}", self.clang.display()))?;
    if !out.status.success() {
      bail!(
        "clang failed (status={})\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
      );
    }
    Ok(())
  }

  pub fn objdump_disassemble_with_relocs(&self, object_path: &Path) -> Result<String> {
    let out = Command::new(&self.llvm_objdump)
      .arg("-dr")
      .arg(object_path)
      .output()
      .with_context(|| format!("failed to run llvm-objdump at {}", self.llvm_objdump.display()))?;
    if !out.status.success() {
      bail!(
        "llvm-objdump -dr failed (status={})\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
      );
    }
    Ok(String::from_utf8(out.stdout)?)
  }

  pub fn objdump_section_headers(&self, object_path: &Path) -> Result<String> {
    let out = Command::new(&self.llvm_objdump)
      .arg("-h")
      .arg(object_path)
      .output()
      .with_context(|| format!("failed to run llvm-objdump at {}", self.llvm_objdump.display()))?;
    if !out.status.success() {
      bail!(
        "llvm-objdump -h failed (status={})\nstdout:\n{}\nstderr:\n{}",
        out.status,
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
      );
    }
    Ok(String::from_utf8(out.stdout)?)
  }
}

fn find_executable(names: &[&str]) -> Option<PathBuf> {
  for name in names {
    if let Some(p) = find_in_path(name) {
      return Some(p);
    }
  }
  None
}

fn find_in_path(exe_name: &str) -> Option<PathBuf> {
  if exe_name.contains(std::path::MAIN_SEPARATOR) {
    let p = PathBuf::from(exe_name);
    return p.is_file().then_some(p);
  }

  let path = std::env::var_os("PATH")?;
  for dir in std::env::split_paths(&path) {
    let candidate = dir.join(exe_name);
    if candidate.is_file() {
      return Some(candidate);
    }

    // Windows compatibility (harmless elsewhere).
    if candidate.extension().is_none() {
      let candidate_exe = candidate.with_extension(OsStr::new("exe"));
      if candidate_exe.is_file() {
        return Some(candidate_exe);
      }
    }
  }
  None
}
