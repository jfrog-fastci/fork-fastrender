#![cfg(target_os = "macos")]

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

use fastrender::sandbox::macos::{apply_renderer_sandbox, MacosSandboxMode};

const CHILD_ENV: &str = "FASTR_TEST_MACOS_SANDBOX_CHILD";
const MODE_ENV: &str = "FASTR_TEST_MACOS_SANDBOX_MODE";
const HOME_FILE_ENV: &str = "FASTR_TEST_MACOS_SANDBOX_HOME_FILE_PATH";

#[test]
fn renderer_sandbox_profiles_enforce_policy() {
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
    // Note: libtest `--exact` matches against the *function* name (integration test crate names are
    // not included in the reported test names), so pass just the function identifier here.
    let test_name = stringify!(renderer_sandbox_profiles_enforce_policy);

    let output = Command::new(&exe)
      .env(CHILD_ENV, "1")
      .env(MODE_ENV, mode)
      .env(HOME_FILE_ENV, &home_file_path)
      .arg("--exact")
      .arg(test_name)
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

fn run_child() {
  let mode = std::env::var(MODE_ENV).expect("child mode env var");
  let home_file_path =
    PathBuf::from(std::env::var_os(HOME_FILE_ENV).expect("child home file env var"));
  let mode = match mode.as_str() {
    "pure" => MacosSandboxMode::PureComputation,
    "relaxed" => MacosSandboxMode::RendererSystemFonts,
    other => panic!("unknown sandbox mode: {other}"),
  };

  // Discover a real system font file path before sandboxing; strict mode denies filesystem reads.
  let font_path = find_system_font_file();

  apply_renderer_sandbox(mode).expect("apply renderer sandbox");

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
  }

  // System fonts should only be readable in the relaxed profile.
  let font_read = std::fs::read(&font_path);
  match mode {
    MacosSandboxMode::PureComputation => {
      assert_permission_denied(
        font_read,
        format!("read system font {}", font_path.display()),
      );
    }
    MacosSandboxMode::RendererSystemFonts => {
      let bytes = font_read.unwrap_or_else(|err| panic!("expected font read to succeed: {err}"));
      assert!(
        !bytes.is_empty(),
        "expected font file to have non-zero length: {}",
        font_path.display()
      );
    }
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
