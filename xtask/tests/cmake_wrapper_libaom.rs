#![cfg(unix)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::TempDir;

fn repo_root() -> PathBuf {
  PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .parent()
    .expect("xtask crate should live under the workspace root")
    .to_path_buf()
}

fn write_fake_cmake(dir: &Path) -> PathBuf {
  let path = dir.join("fake_cmake.sh");
  fs::write(
    &path,
    r#"#!/usr/bin/env sh
printf '%s\n' "$@"
"#,
  )
  .expect("write fake cmake");

  #[cfg(unix)]
  {
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(&path).expect("stat fake cmake").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&path, perms).expect("chmod fake cmake");
  }

  path
}

fn run_wrapper(args: &[&Path], env: &[(&str, &str)], remove_env: &[&str]) -> Vec<String> {
  let temp = TempDir::new().expect("tempdir");
  let fake_cmake = write_fake_cmake(temp.path());

  let wrapper = repo_root().join("tools/cmake_wrapper.sh");
  assert!(wrapper.is_file(), "missing wrapper at {}", wrapper.display());

  let mut cmd = Command::new("bash");
  cmd.current_dir(temp.path());
  cmd.arg(&wrapper);
  for arg in args {
    cmd.arg(arg);
  }
  cmd.env("FASTR_REAL_CMAKE", &fake_cmake);
  for (k, v) in env {
    cmd.env(k, v);
  }
  for key in remove_env {
    cmd.env_remove(key);
  }

  let output = cmd.output().expect("run cmake wrapper");
  assert!(
    output.status.success(),
    "cmake wrapper should exit successfully\nstdout:\n{}\nstderr:\n{}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );

  String::from_utf8_lossy(&output.stdout)
    .lines()
    .map(|line| line.to_string())
    .collect()
}

#[test]
fn libaom_configure_injects_generic_and_disables_asm() {
  let temp = TempDir::new().expect("tempdir");
  let src = temp.path().join("vendor");
  let build = temp.path().join("build");
  fs::create_dir_all(&src).expect("mkdir src");
  fs::create_dir_all(&build).expect("mkdir build");

  let args = [
    Path::new("-S"),
    src.as_path(),
    Path::new("-B"),
    build.as_path(),
  ];

  let cmake_args = run_wrapper(
    &args,
    &[("CARGO_PKG_NAME", "libaom-sys")],
    &["AOM_TARGET_CPU", "CMAKE_TOOLCHAIN_FILE"],
  );

  assert!(
    cmake_args.iter().any(|arg| arg == "-DAOM_TARGET_CPU=generic"),
    "expected wrapper to inject -DAOM_TARGET_CPU=generic, got:\n{cmake_args:?}"
  );
  for flag in ["-DENABLE_ASM=0", "-DENABLE_NASM=0", "-DENABLE_YASM=0"] {
    assert!(
      cmake_args.iter().any(|arg| arg == flag),
      "expected wrapper to inject {flag}, got:\n{cmake_args:?}"
    );
  }

  let toolchain_arg = cmake_args
    .iter()
    .find(|arg| arg.starts_with("-DCMAKE_TOOLCHAIN_FILE="))
    .expect("expected wrapper to inject -DCMAKE_TOOLCHAIN_FILE=...");
  let toolchain_path = PathBuf::from(toolchain_arg.trim_start_matches("-DCMAKE_TOOLCHAIN_FILE="));
  assert!(
    toolchain_path.is_absolute(),
    "toolchain file should be an absolute path; got {toolchain_path:?} from {toolchain_arg:?}"
  );

  let expected_toolchain = repo_root().join("tools/cmake/aom_target_cpu_generic.cmake");
  assert_eq!(
    toolchain_path.canonicalize().expect("canonicalize toolchain"),
    expected_toolchain
      .canonicalize()
      .expect("canonicalize expected toolchain"),
    "toolchain file path should point at tools/cmake/aom_target_cpu_generic.cmake"
  );
}

#[test]
fn libaom_configure_respects_env_opt_out() {
  let temp = TempDir::new().expect("tempdir");
  let src = temp.path().join("vendor");
  let build = temp.path().join("build");
  fs::create_dir_all(&src).expect("mkdir src");
  fs::create_dir_all(&build).expect("mkdir build");

  let args = [
    Path::new("-S"),
    src.as_path(),
    Path::new("-B"),
    build.as_path(),
  ];

  let cmake_args = run_wrapper(
    &args,
    &[("CARGO_PKG_NAME", "libaom-sys"), ("AOM_TARGET_CPU", "x86_64")],
    &["CMAKE_TOOLCHAIN_FILE"],
  );

  assert!(
    cmake_args.iter().any(|arg| arg == "-DAOM_TARGET_CPU=x86_64"),
    "expected wrapper to forward AOM_TARGET_CPU from env, got:\n{cmake_args:?}"
  );
  assert!(
    !cmake_args.iter().any(|arg| arg.starts_with("-DENABLE_ASM=")),
    "wrapper should not force-disable asm when opting out via env, got:\n{cmake_args:?}"
  );
  assert!(
    !cmake_args
      .iter()
      .any(|arg| arg.starts_with("-DCMAKE_TOOLCHAIN_FILE=")),
    "wrapper should not inject its toolchain file when opting out via env, got:\n{cmake_args:?}"
  );
}

#[test]
fn libaom_configure_respects_explicit_target_cpu_arg() {
  let temp = TempDir::new().expect("tempdir");
  let src = temp.path().join("vendor");
  let build = temp.path().join("build");
  fs::create_dir_all(&src).expect("mkdir src");
  fs::create_dir_all(&build).expect("mkdir build");

  let args = [
    Path::new("-S"),
    src.as_path(),
    Path::new("-B"),
    build.as_path(),
    Path::new("-DAOM_TARGET_CPU=x86_64"),
  ];

  let cmake_args = run_wrapper(
    &args,
    &[("CARGO_PKG_NAME", "libaom-sys")],
    &["AOM_TARGET_CPU", "CMAKE_TOOLCHAIN_FILE"],
  );

  assert!(
    cmake_args.iter().any(|arg| arg == "-DAOM_TARGET_CPU=x86_64"),
    "expected wrapper to forward explicit -DAOM_TARGET_CPU arg, got:\n{cmake_args:?}"
  );
  assert!(
    !cmake_args.iter().any(|arg| arg.starts_with("-DENABLE_ASM=")),
    "wrapper should not force-disable asm when target cpu is explicitly set, got:\n{cmake_args:?}"
  );
  assert!(
    !cmake_args
      .iter()
      .any(|arg| arg.starts_with("-DCMAKE_TOOLCHAIN_FILE=")),
    "wrapper should not inject its toolchain file when target cpu is explicitly set, got:\n{cmake_args:?}"
  );
}

#[test]
fn libaom_configure_detects_by_argv_when_cargo_metadata_is_missing() {
  let temp = TempDir::new().expect("tempdir");
  let src = temp.path().join("libaom-sys-vendor");
  let build = temp.path().join("build");
  fs::create_dir_all(&src).expect("mkdir src");
  fs::create_dir_all(&build).expect("mkdir build");

  let args = [
    Path::new("-S"),
    src.as_path(),
    Path::new("-B"),
    build.as_path(),
  ];

  let cmake_args = run_wrapper(
    &args,
    &[],
    &[
      "AOM_TARGET_CPU",
      "CARGO_MANIFEST_DIR",
      "CARGO_PKG_NAME",
      "CMAKE_TOOLCHAIN_FILE",
    ],
  );

  assert!(
    cmake_args.iter().any(|arg| arg == "-DAOM_TARGET_CPU=generic"),
    "expected wrapper to inject -DAOM_TARGET_CPU=generic, got:\n{cmake_args:?}"
  );
  for flag in ["-DENABLE_ASM=0", "-DENABLE_NASM=0", "-DENABLE_YASM=0"] {
    assert!(
      cmake_args.iter().any(|arg| arg == flag),
      "expected wrapper to inject {flag}, got:\n{cmake_args:?}"
    );
  }
}
