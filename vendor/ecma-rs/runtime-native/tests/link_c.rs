use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn has_command(cmd: &str) -> bool {
  Command::new(cmd).arg("--version").output().is_ok()
}

fn find_staticlib(target_dir: &Path, profile: &str) -> PathBuf {
  let direct = target_dir.join(profile).join("libruntime_native.a");
  if direct.is_file() {
    return direct;
  }

  let deps_dir = target_dir.join(profile).join("deps");
  let mut newest: Option<(std::time::SystemTime, PathBuf)> = None;
  if let Ok(entries) = fs::read_dir(&deps_dir) {
    for entry in entries.flatten() {
      let path = entry.path();
      let Some(file_name) = path.file_name().and_then(OsStr::to_str) else {
        continue;
      };
      if !(file_name.starts_with("libruntime_native") && file_name.ends_with(".a")) {
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
    "failed to find runtime-native staticlib at {} (checked {} and {})",
    target_dir.display(),
    direct.display(),
    deps_dir.display()
  );
}

fn run_checked(mut cmd: Command) {
  let output = cmd.output().unwrap_or_else(|err| panic!("failed to run {cmd:?}: {err}"));
  if !output.status.success() {
    panic!(
      "command failed: {cmd:?}\nexit={}\nstdout:\n{}\nstderr:\n{}",
      output.status,
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr)
    );
  }
}

#[test]
fn link_c_against_runtime_native_staticlib() {
  if !cfg!(target_os = "linux") {
    eprintln!("skipping: C link test currently only supported on Linux");
    return;
  }

  if !has_command("clang-18") {
    eprintln!("skipping: clang-18 not found in PATH");
    return;
  }

  // `clang-18 -fuse-ld=lld-18` expects `ld.lld-18` to be available.
  if !has_command("ld.lld-18") {
    eprintln!("skipping: ld.lld-18 not found in PATH");
    return;
  }

  let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let stackmaps_ld = manifest_dir.join("stackmaps.ld");
  if !stackmaps_ld.is_file() {
    panic!("stackmaps.ld not found at {}", stackmaps_ld.display());
  }

  // runtime-native/ -> ecma-rs/ -> vendor/ -> repo root
  let repo_root = manifest_dir
    .ancestors()
    .nth(3)
    .expect("CARGO_MANIFEST_DIR should have at least 3 parents");
  let run_limited = repo_root.join("scripts/run_limited.sh");
  if !run_limited.is_file() {
    panic!("run_limited.sh not found at {}", run_limited.display());
  }

  let workspace_root = manifest_dir
    .parent()
    .expect("runtime-native should be nested under the ecma-rs workspace");
  let target_dir = std::env::var_os("CARGO_TARGET_DIR")
    .map(PathBuf::from)
    .unwrap_or_else(|| workspace_root.join("target"));

  let profile = if cfg!(debug_assertions) { "debug" } else { "release" };
  let staticlib = find_staticlib(&target_dir, profile);

  // Verify the archive contains the exported symbols (best-effort; linking below is the real check).
  if has_command("nm") {
    let out = Command::new("nm")
      .arg("-g")
      .arg(&staticlib)
      .output()
      .expect("failed to run nm");
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
      text.contains("rt_gc_collect"),
      "expected rt_gc_collect to be present in {}\n{}",
      staticlib.display(),
      text
    );
  }

  let tmp = tempfile::tempdir().expect("tempdir");
  let obj = tmp.path().join("link_c.o");
  let exe = tmp.path().join("link_c_bin");

  let c_src = manifest_dir.join("tests/link_c/main.c");

  run_checked({
    let mut cmd = Command::new("clang-18");
    cmd.arg("-c").arg(&c_src).arg("-o").arg(&obj);
    cmd
  });

  run_checked({
    let mut cmd = Command::new("clang-18");
    cmd
      .arg("-fuse-ld=lld-18")
      .arg(format!("-Wl,-T,{}", stackmaps_ld.display()))
      .arg(&obj)
      .arg(&staticlib)
      .arg("-o")
      .arg(&exe);
    cmd
  });

  // Run under the repo's resource limiter to keep CI deterministic.
  run_checked({
    let mut cmd = Command::new("bash");
    cmd
      .arg(&run_limited)
      .arg("--as")
      .arg("512M")
      .arg("--cpu")
      .arg("10")
      .arg("--")
      .arg(&exe);
    cmd
  });
}
