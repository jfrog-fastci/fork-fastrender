use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use fastrender::sandbox as sandbox_mod;
use fastrender::sandbox::macos::{
  apply_renderer_sandbox, sandbox_check_mach_lookup, MacosSandboxMode, MacosSandboxStatus,
};

const CHILD_ENV: &str = "FASTR_TEST_MACOS_SANDBOX_CHILD";
const MODE_ENV: &str = "FASTR_TEST_MACOS_SANDBOX_MODE";
const HOME_FILE_ENV: &str = "FASTR_TEST_MACOS_SANDBOX_HOME_FILE_PATH";

#[test]
fn renderer_sandbox_profiles_enforce_policy() {
  let test_name = crate::common::libtest::exact_test_name(
    module_path!(),
    stringify!(renderer_sandbox_profiles_enforce_policy),
  );
  let is_child = std::env::var_os(CHILD_ENV).is_some();
  if is_child {
    run_child();
    return;
  }

  let home_file = create_home_test_file().expect("create home test file");
  let home_file_path = home_file.path().join("fastrender-seatbelt-home.txt");
  std::fs::write(&home_file_path, b"fastrender seatbelt home file").expect("write home test file");

  for mode in ["pure", "relaxed"] {
    let exe = std::env::current_exe().expect("test exe path");

    let output = Command::new(&exe)
      .env(CHILD_ENV, "1")
      .env(MODE_ENV, mode)
      .env(HOME_FILE_ENV, &home_file_path)
      .arg("--exact")
      .arg(&test_name)
      .arg("--nocapture")
      .output()
      .unwrap_or_else(|err| panic!("spawn child process: {err}"));

    assert!(
      output.status.success(),
      "child sandbox test ({mode}) failed (stdout={}, stderr={})",
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr)
    );
  }
}

#[test]
fn sandbox_env_disable_skips_seatbelt() {
  const CHILD_ENV: &str = "FASTR_TEST_MACOS_SANDBOX_DISABLE_CHILD";
  const SENTINEL: &str = "FASTR_TEST_MACOS_SANDBOX_DISABLE_OK";

  if std::env::var_os(CHILD_ENV).is_some() {
    // Even though we request a strict sandbox, the env var should disable sandboxing entirely.
    sandbox_mod::apply_pure_computation_sandbox()
      .expect("apply_pure_computation_sandbox should be a no-op when disabled");

    // If Seatbelt was applied, these operations would be denied under pure-computation.
    let passwd = std::fs::read("/etc/passwd").or_else(|err| {
      if err.kind() == io::ErrorKind::NotFound {
        std::fs::read("/private/etc/passwd")
      } else {
        Err(err)
      }
    });
    assert!(
      passwd.is_ok(),
      "expected /etc/passwd read to succeed when sandbox is disabled, got {passwd:?}"
    );

    let listener = std::net::TcpListener::bind("127.0.0.1:0");
    assert!(
      listener.is_ok(),
      "expected localhost bind to succeed when sandbox is disabled, got {listener:?}"
    );

    let status = Command::new("/usr/bin/true").status();
    assert!(
      status.as_ref().is_ok_and(|s| s.success()),
      "expected /usr/bin/true to be spawnable when sandbox is disabled, got {status:?}"
    );

    println!("{SENTINEL}");
    return;
  }

  let exe = std::env::current_exe().expect("test exe path");
  let test_name = crate::common::libtest::exact_test_name(
    module_path!(),
    stringify!(sandbox_env_disable_skips_seatbelt),
  );
  let output = Command::new(&exe)
    .env(CHILD_ENV, "1")
    .env("FASTR_DISABLE_RENDERER_SANDBOX", "1")
    .arg("--exact")
    .arg(&test_name)
    .arg("--nocapture")
    .output()
    .expect("spawn sandbox env override child");

  assert!(
    output.status.success(),
    "child sandbox env override should exit successfully (stdout={}, stderr={})",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
  assert!(
    String::from_utf8_lossy(&output.stdout).contains(SENTINEL),
    "expected sentinel; stdout={}, stderr={}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
}

#[test]
fn sandbox_env_profile_overrides_strict_to_system_fonts() {
  const CHILD_ENV: &str = "FASTR_TEST_MACOS_SANDBOX_PROFILE_OVERRIDE_CHILD";
  const SENTINEL: &str = "FASTR_TEST_MACOS_SANDBOX_PROFILE_OVERRIDE_OK";

  if std::env::var_os(CHILD_ENV).is_some() {
    // Discover a real system font file path before sandboxing; strict mode denies filesystem reads.
    let font_path = find_system_font_file();

    // Even though this is the strict entrypoint, the env var should select the relaxed system-fonts
    // profile so that font discovery/loading can proceed for debugging.
    sandbox_mod::apply_pure_computation_sandbox()
      .expect("apply_pure_computation_sandbox should respect env profile override");

    let bytes = std::fs::read(&font_path)
      .unwrap_or_else(|err| panic!("expected system font read to succeed under override: {err}"));
    assert!(
      !bytes.is_empty(),
      "expected non-empty font bytes from {}",
      font_path.display()
    );

    // Even in the relaxed profile, sensitive filesystem + network access should remain blocked.
    assert_permission_denied(std::fs::read("/etc/passwd"), "read /etc/passwd");
    assert_permission_denied(std::net::TcpListener::bind("127.0.0.1:0"), "bind localhost");

    println!("{SENTINEL}");
    return;
  }

  let exe = std::env::current_exe().expect("test exe path");
  let test_name = crate::common::libtest::exact_test_name(
    module_path!(),
    stringify!(sandbox_env_profile_overrides_strict_to_system_fonts),
  );
  let output = Command::new(&exe)
    .env(CHILD_ENV, "1")
    .env("FASTR_MACOS_RENDERER_SANDBOX", "system-fonts")
    .arg("--exact")
    .arg(&test_name)
    .arg("--nocapture")
    .output()
    .expect("spawn sandbox env profile override child");

  assert!(
    output.status.success(),
    "child sandbox env profile override should exit successfully (stdout={}, stderr={})",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
  assert!(
    String::from_utf8_lossy(&output.stdout).contains(SENTINEL),
    "expected sentinel; stdout={}, stderr={}",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
}

fn run_child() {
  let mode = std::env::var(MODE_ENV).expect("child mode env var");
  let home_file_path =
    PathBuf::from(std::env::var_os(HOME_FILE_ENV).expect("child home file env var"));
  let mode = match mode.as_str() {
    "pure" => MacosSandboxMode::PureComputation,
    "relaxed" => MacosSandboxMode::RendererSystemFonts,
    other => panic!("unknown sandbox mode: {other}"),
  };

  let status = apply_renderer_sandbox(mode).expect("apply renderer sandbox");
  assert!(
    matches!(
      status,
      MacosSandboxStatus::Applied | MacosSandboxStatus::AlreadySandboxed
    ),
    "unexpected renderer sandbox status: {status:?}"
  );
  if matches!(status, MacosSandboxStatus::AlreadySandboxed) {
    eprintln!(
      "note: process was already sandboxed before applying renderer profile; skipping profile-specific assertions"
    );
    return;
  }

  // Defense-in-depth: "no network" should not be bypassable by talking to system daemons over
  // mach/XPC (e.g. `nsurlsessiond` can perform network on behalf of the client).
  assert_eq!(
    sandbox_check_mach_lookup("com.apple.nsurlsessiond")
      .expect("sandbox_check mach-lookup com.apple.nsurlsessiond"),
    false,
    "expected sandbox to deny mach-lookup to com.apple.nsurlsessiond",
  );

  // HOME should never be readable/writable, even in the relaxed profile (system fonts only).
  assert_permission_denied(
    std::fs::read(&home_file_path),
    format!("read home file {}", home_file_path.display()),
  );
  assert_permission_denied(
    std::fs::write(&home_file_path, b"sandbox-write-probe"),
    format!("write home file {}", home_file_path.display()),
  );

  // Sensitive system files should not be readable.
  assert_permission_denied(std::fs::read("/etc/passwd"), "read /etc/passwd");
  // Filesystem metadata and directory listings can also leak sensitive information.
  assert_permission_denied(std::fs::metadata("/etc/passwd"), "metadata /etc/passwd");
  assert_permission_denied(std::fs::read_dir("/etc"), "read_dir /etc");
  assert_permission_denied(std::fs::canonicalize("/etc/passwd"), "canonicalize /etc/passwd");

  // Network should be denied in both modes.
  assert_permission_denied(std::net::TcpListener::bind("127.0.0.1:0"), "bind localhost");
  assert_permission_denied(std::net::UdpSocket::bind("127.0.0.1:0"), "bind UDP localhost");

  // Relaxed mode should still deny user/home filesystem metadata access, but allow system font
  // directory listing so font discovery can run.
  if mode == MacosSandboxMode::RendererSystemFonts {
    assert_permission_denied(
      std::fs::metadata(&home_file_path),
      format!("metadata {}", home_file_path.display()),
    );

    let mut entries =
      std::fs::read_dir("/System/Library/Fonts").expect("read_dir system fonts allowed");
    if let Some(entry) = entries.next() {
      entry.expect("read first entry in system font dir");
    }

    // System fonts should be readable in the relaxed profile.
    let font_path = find_system_font_file();
    let bytes = std::fs::read(&font_path)
      .unwrap_or_else(|err| panic!("expected font read to succeed (path={}): {err}", font_path.display()));
    assert!(
      !bytes.is_empty(),
      "expected font file to have non-zero length: {}",
      font_path.display()
    );
  } else {
    // Strict mode should deny system font enumeration.
    assert_permission_denied(
      std::fs::read_dir("/System/Library/Fonts"),
      "read_dir /System/Library/Fonts",
    );
  }
}

fn assert_permission_denied<T>(result: Result<T, io::Error>, context: impl std::fmt::Display) {
  match result {
    Ok(_) => panic!("expected permission denied for {context}"),
    Err(err) => {
      let raw = err.raw_os_error();
      let is_perm = err.kind() == io::ErrorKind::PermissionDenied
        || matches!(raw, Some(libc::EPERM) | Some(libc::EACCES));
      assert!(
        is_perm,
        "expected permission denied for {context}, got kind={:?} raw_os_error={raw:?} err={err}",
        err.kind(),
      );
    }
  }
}

fn find_system_font_file() -> PathBuf {
  fn is_font_file(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(OsStr::to_str) else {
      return false;
    };
    matches!(ext.to_ascii_lowercase().as_str(), "ttf" | "ttc" | "otf")
  }

  // Avoid hardcoding a specific system font filename: the set varies across macOS versions and
  // installations. Instead, pick the first font file in a small set of well-known font
  // directories.
  const FONT_DIRS: [&str; 3] = [
    "/System/Library/Fonts",
    "/System/Library/Fonts/Supplemental",
    "/Library/Fonts",
  ];

  for dir in FONT_DIRS {
    let Ok(entries) = std::fs::read_dir(dir) else {
      continue;
    };

    // `read_dir` ordering is unspecified; sort entries so strict/relaxed runs choose the same font.
    let mut entries: Vec<_> = entries.filter_map(Result::ok).collect();
    entries.sort_by(|a, b| a.file_name().cmp(&b.file_name()));

    for entry in entries {
      let ty = match entry.file_type() {
        Ok(ty) => ty,
        Err(_) => continue,
      };
      if !ty.is_file() {
        continue;
      }
      let path = entry.path();
      if is_font_file(&path) {
        return path;
      }
    }
  }

  panic!(
    "expected to find at least one system font file (.ttf/.otf/.ttc) in one of: {}",
    FONT_DIRS.join(", ")
  );
}

fn create_home_test_file() -> io::Result<tempfile::TempDir> {
  let home = std::env::var_os("HOME").ok_or_else(|| {
    io::Error::new(io::ErrorKind::NotFound, "HOME env var not set for sandbox test")
  })?;
  let home_dir = PathBuf::from(home);
  tempfile::Builder::new()
    .prefix("fastr-seatbelt-home-")
    .tempdir_in(&home_dir)
}
