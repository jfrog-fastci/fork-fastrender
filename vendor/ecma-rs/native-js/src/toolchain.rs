use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::process::Stdio;

use anyhow::{anyhow, bail, Context, Result};

/// Toolchain configuration for external LLVM/Clang tooling used by native-js.
///
/// This is intentionally "loose": some tools are optional depending on the link mode.
/// For example, `llvm-objcopy` is only required when we need to rewrite input objects to
/// relocate `.llvm_stackmaps` into RELRO-friendly data sections (PIE + lld).
#[derive(Clone, Debug)]
pub struct Toolchain {
  /// Path to `clang` (or `clang-18`).
  pub clang: PathBuf,
  /// Argument to `clang -fuse-ld=<...>` if an `ld.lld` driver is available (e.g. `lld-18` or `lld`).
  ///
  /// This is detected by looking for `ld.lld-18`/`ld.lld` in PATH.
  pub lld_fuse_arg: Option<String>,
  /// Path to `llvm-objcopy` (or `llvm-objcopy-18`) if available.
  pub llvm_objcopy: Option<PathBuf>,
  /// Path to `llvm-objdump` (or `llvm-objdump-18`) if available.
  pub llvm_objdump: Option<PathBuf>,
  /// Optional sysroot path forwarded to `clang` as `--sysroot=<path>`.
  pub sysroot: Option<PathBuf>,
  /// Extra arguments forwarded to `clang` during linking.
  pub extra_link_args: Vec<String>,
}

/// Backwards-compatible alias.
pub type LlvmToolchain = Toolchain;

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

impl Toolchain {
  pub fn detect() -> Result<Self> {
    Self::detect_with_overrides(None, None, None, None, Vec::new())
  }

  pub fn detect_with_overrides(
    clang: Option<PathBuf>,
    llvm_objcopy: Option<PathBuf>,
    llvm_objdump: Option<PathBuf>,
    sysroot: Option<PathBuf>,
    extra_link_args: Vec<String>,
  ) -> Result<Self> {
    let clang = match clang {
      Some(path) => resolve_executable(path, "clang")?,
      None => find_executable(&["clang-18", "clang"]).ok_or_else(|| {
        anyhow!(missing_tool_message(
          "clang",
          &["clang-18", "clang"],
          Some("install `clang-18` (LLVM 18) or pass an explicit --clang <PATH>"),
        ))
      })?,
    };

    let llvm_objcopy = match llvm_objcopy {
      Some(path) => Some(resolve_executable(path, "llvm-objcopy")?),
      None => find_executable(&["llvm-objcopy-18", "llvm-objcopy"]),
    };

    let llvm_objdump = match llvm_objdump {
      Some(path) => Some(resolve_executable(path, "llvm-objdump")?),
      None => find_executable(&["llvm-objdump-18", "llvm-objdump"]),
    };

    if let Some(sr) = sysroot.as_ref() {
      if !sr.exists() {
        bail!("--sysroot does not exist: {}", sr.display());
      }
    }

    Ok(Self {
      clang,
      lld_fuse_arg: detect_lld_fuse_arg(),
      llvm_objcopy,
      llvm_objdump,
      sysroot,
      extra_link_args,
    })
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
      // Test fixtures sometimes use a slightly different LLVM target triple (e.g.
      // `x86_64-unknown-linux-gnu`) than the host `clang -dumpmachine` triple
      // (e.g. `x86_64-pc-linux-gnu`). Clang will override the module triple during
      // compilation and emit a warning, which is just noise for these tests.
      .arg("-Wno-override-module")
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
    let objdump = self
      .llvm_objdump
      .as_ref()
      .ok_or_else(|| anyhow!("llvm-objdump is not configured (expected `llvm-objdump-18` or `llvm-objdump` in PATH)"))?;
    let out = Command::new(objdump)
      .arg("-dr")
      .arg(object_path)
      .output()
      .with_context(|| format!("failed to run llvm-objdump at {}", objdump.display()))?;
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
    let objdump = self
      .llvm_objdump
      .as_ref()
      .ok_or_else(|| anyhow!("llvm-objdump is not configured (expected `llvm-objdump-18` or `llvm-objdump` in PATH)"))?;
    let out = Command::new(objdump)
      .arg("-h")
      .arg(object_path)
      .output()
      .with_context(|| format!("failed to run llvm-objdump at {}", objdump.display()))?;
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

fn missing_tool_message(tool: &str, candidates: &[&str], extra_hint: Option<&str>) -> String {
  let mut msg = String::new();
  msg.push_str(&format!("missing required tool: {tool}\n"));
  msg.push_str("candidates tried:\n");
  for cand in candidates {
    msg.push_str(&format!("  - {cand}\n"));
  }
  msg.push_str("hint: install LLVM 18 tools (e.g. `clang-18`, `lld-18`, `llvm-objcopy-18`) and ensure they are in PATH");
  if let Some(extra) = extra_hint {
    msg.push_str(&format!("\n{extra}"));
  }
  msg
}

fn find_executable(names: &[&str]) -> Option<PathBuf> {
  for name in names {
    if let Some(p) = find_in_path(name) {
      // Ensure the resolved tool actually runs. Some environments may have a stray file in PATH
      // (e.g. a non-executable wrapper), which `Path::is_file` would accept but `Command::new`
      // would fail to spawn.
      if Command::new(&p)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
      {
        return Some(p);
      }
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

fn detect_lld_fuse_arg() -> Option<String> {
  if find_executable(&["ld.lld-18"]).is_some() {
    Some("lld-18".to_string())
  } else if find_executable(&["ld.lld"]).is_some() {
    Some("lld".to_string())
  } else {
    None
  }
}

fn resolve_executable(path: PathBuf, tool: &str) -> Result<PathBuf> {
  let path = if path.as_os_str().is_empty() {
    bail!("invalid empty path for {tool}");
  } else if path.to_string_lossy().contains(std::path::MAIN_SEPARATOR) {
    path
  } else {
    // Allow passing a bare tool name (e.g. `--clang clang-18`) and resolve it via PATH.
    find_in_path(&path.to_string_lossy()).ok_or_else(|| {
      anyhow!(missing_tool_message(
        tool,
        &[path.to_string_lossy().as_ref()],
        None,
      ))
    })?
  };

  if !path.is_file() {
    bail!("{} does not exist or is not a file: {}", tool, path.display());
  }

  let out = Command::new(&path)
    .arg("--version")
    .stdin(Stdio::null())
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .output()
    .with_context(|| format!("failed to run {tool} at {}", path.display()))?;
  if !out.status.success() {
    bail!("{tool} at {} failed --version with status {}", path.display(), out.status);
  }

  Ok(path)
}
