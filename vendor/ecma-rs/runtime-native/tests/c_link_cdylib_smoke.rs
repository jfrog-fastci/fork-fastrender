use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Debug)]
struct Cc {
  program: String,
  args: Vec<String>,
}

impl Cc {
  fn new(program: impl Into<String>) -> Self {
    Self {
      program: program.into(),
      args: Vec::new(),
    }
  }

  fn with_args(program: impl Into<String>, args: Vec<String>) -> Self {
    Self {
      program: program.into(),
      args,
    }
  }
}

fn find_c_compiler() -> Option<Cc> {
  // Prefer $CC when set (common in CI / cross toolchains).
  if let Ok(cc) = std::env::var("CC") {
    let parts: Vec<&str> = cc.split_whitespace().collect();
    if let Some((program, args)) = parts.split_first() {
      return Some(Cc::with_args(
        (*program).to_string(),
        args.iter().map(|s| (*s).to_string()).collect(),
      ));
    }
  }

  // Ubuntu images usually provide `cc`. Fall back to clang/gcc when needed.
  for candidate in ["cc", "clang", "gcc"] {
    if Command::new(candidate)
      .arg("--version")
      .stdout(std::process::Stdio::null())
      .stderr(std::process::Stdio::null())
      .status()
      .is_ok_and(|s| s.success())
    {
      return Some(Cc::new(candidate));
    }
  }

  None
}

fn workspace_root() -> PathBuf {
  // runtime-native/ is a workspace member; workspace root is its parent.
  Path::new(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("runtime-native should live at <workspace>/runtime-native")
    .to_path_buf()
}

fn target_dir() -> PathBuf {
  std::env::var_os("CARGO_TARGET_DIR")
    .map(PathBuf::from)
    .unwrap_or_else(|| workspace_root().join("target"))
}

fn find_cdylib(target_dir: &Path, profile: &str) -> PathBuf {
  let direct = target_dir.join(profile).join("libruntime_native.so");
  let deps_dir = target_dir.join(profile).join("deps");
  let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;

  if direct.is_file() {
    let mtime = fs::metadata(&direct)
      .and_then(|meta| meta.modified())
      .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
    newest = Some((mtime, direct.clone()));
  }

  if let Ok(entries) = fs::read_dir(&deps_dir) {
    for entry in entries.flatten() {
      let path = entry.path();
      let Some(file_name) = path.file_name().and_then(|s| s.to_str()) else {
        continue;
      };
      if !(file_name.starts_with("libruntime_native") && file_name.ends_with(".so")) {
        continue;
      }
      let mtime = fs::metadata(&path)
        .and_then(|meta| meta.modified())
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
      match newest {
        Some((best, _)) if mtime <= best => {}
        _ => newest = Some((mtime, path)),
      }
    }
  }

  if let Some((_, path)) = newest {
    return path;
  }

  panic!(
    "failed to find runtime-native cdylib at {} (checked {} and {})",
    target_dir.display(),
    direct.display(),
    deps_dir.display()
  );
}

#[test]
fn c_can_link_and_run_against_runtime_native_cdylib() {
  if !cfg!(target_os = "linux") {
    eprintln!("skipping: cdylib C link smoke test only runs on Linux");
    return;
  }

  let Some(cc) = find_c_compiler() else {
    eprintln!("skipping: no C compiler (`cc`/`clang`/`gcc`) available");
    return;
  };

  let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
  let cdylib = find_cdylib(&target_dir(), profile);
  let so_dir = cdylib.parent().expect("cdylib should have a parent dir");

  let tmp = tempfile::tempdir().expect("tempdir");
  let c_path = tmp.path().join("smoke_cdylib.c");
  let bin_path = tmp.path().join("smoke_cdylib");

  fs::write(
    &c_path,
    r#"
#define _POSIX_C_SOURCE 200809L
#include "runtime_native.h"
#include <stdint.h>

// `place-safepoints` emits calls to a symbol named `gc.safepoint_poll`.
// This is not a valid C identifier, so bind it via an asm label.
extern void gc_safepoint_poll(void) __asm__("gc.safepoint_poll");

int main(void) {
  // Use External kind; this test only validates dynamic linking works.
  rt_thread_init(3);
  rt_gc_safepoint();
  // The slow-path entrypoint is implemented in platform-specific assembly (and has historically
  // been easy to accidentally omit from `cdylib` exports). Call it with an even epoch so it returns
  // immediately, but still exercises the symbol resolution + call path.
  rt_gc_safepoint_slow((uint64_t)0);
  // Exercise `gc.safepoint_poll` resolution too. With no active stop-the-world request it should
  // return immediately.
  gc_safepoint_poll();
  rt_thread_deinit();
  return 0;
}
"#,
  )
  .expect("write smoke_cdylib.c");

  let include_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("include");
  let so_dir_abs = fs::canonicalize(so_dir).unwrap_or_else(|_| so_dir.to_path_buf());

  // Like the staticlib C smoke test, define `__start_llvm_stackmaps` / `__stop_llvm_stackmaps` (and
  // aliases) in the final executable so stackmaps-driven APIs can be used when the caller also links
  // LLVM-generated code.
  let stackmaps_ld = {
    let linker_version_out = Command::new(&cc.program)
      .args(&cc.args)
      .args(["-Wl,--version"])
      .output()
      .unwrap_or_else(|e| panic!("failed to query linker version via {}: {e}", cc.program));
    let linker_version = format!(
      "{}{}",
      String::from_utf8_lossy(&linker_version_out.stdout),
      String::from_utf8_lossy(&linker_version_out.stderr),
    );
    let cc_uses_lld = linker_version.to_ascii_lowercase().contains("lld");

    let lld_script = Path::new(env!("CARGO_MANIFEST_DIR"))
      .join("link")
      .join("stackmaps.ld");
    let gnuld_script = Path::new(env!("CARGO_MANIFEST_DIR"))
      .join("link")
      .join("stackmaps_gnuld.ld");
    if cc_uses_lld { lld_script } else { gnuld_script }
  };

  let mut cmd = Command::new(&cc.program);
  cmd
    .args(&cc.args)
    .arg("-std=c99")
    .arg("-I")
    .arg(&include_dir)
    .arg(&c_path)
    .arg("-L")
    .arg(so_dir)
    .arg("-lruntime_native")
    .arg(format!("-Wl,-rpath,{}", so_dir_abs.display()))
    .arg(format!("-Wl,-T,{}", stackmaps_ld.display()))
    .arg("-o")
    .arg(&bin_path);

  let compile = cmd.status().expect("compile + link smoke_cdylib.c");
  assert!(
    compile.success(),
    "C compile/link against cdylib failed with status: {compile:?}"
  );

  let run = Command::new(&bin_path)
    .env("LD_LIBRARY_PATH", &so_dir_abs)
    // Make the smoke binary deterministic and avoid flakiness from spawning a
    // large number of worker threads on first use (`rt_ensure_init`).
    .env("ECMA_RS_RUNTIME_NATIVE_THREADS", "1")
    .env("ECMA_RS_RUNTIME_NATIVE_BLOCKING_THREADS", "1")
    .status()
    .expect("run linked C binary");

  assert!(run.success(), "C binary exited non-zero: {run:?}");
}
