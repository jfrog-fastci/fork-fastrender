//! Regression test: the renderer sandbox must still permit core offline rendering.
//!
//! This is intended to catch cases where the Linux sandbox (seccomp/landlock denylist) accidentally
//! blocks syscalls required by the render pipeline (thread primitives, time syscalls, etc.).

#[cfg(target_os = "linux")]
use std::process::Command;

#[cfg(target_os = "linux")]
const CHILD_ENV: &str = "FASTR_TEST_RENDERER_SANDBOX_CHILD";

#[cfg(target_os = "linux")]
#[test]
fn sandboxed_minimal_offline_render_succeeds() {
  let test_name = "sandboxed_minimal_offline_render_succeeds";
  let is_child = std::env::var_os(CHILD_ENV).is_some();
  if is_child {
    sandboxed_child_entrypoint();
    return;
  }

  let exe = std::env::current_exe().expect("resolve current test executable");
  let output = Command::new(exe)
    .env(CHILD_ENV, "1")
    // Avoid a large libtest threadpool: the sandbox applies to all threads when TSYNC is supported,
    // and when TSYNC is unavailable we must avoid spawning additional threads before sandboxing.
    .env("RUST_TEST_THREADS", "1")
    .arg("--test-threads=1")
    // Run only this test in the child process. The renderer sandbox is process-global, so we must
    // avoid concurrently executing any other tests in the same binary.
    .arg("--exact")
    .arg(test_name)
    .arg("--nocapture")
    .output()
    .expect("spawn sandboxed child test process");
  assert!(
    output.status.success(),
    "sandboxed child should exit successfully (stdout={}, stderr={})",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
}

#[cfg(target_os = "linux")]
fn sandboxed_child_entrypoint() {
  use std::ffi::CString;
  use std::io;
  use std::os::unix::ffi::OsStrExt;
  use std::time::Duration;

  // Create a probe file *before* sandboxing so we can deterministically check that filesystem
  // access is blocked afterwards (without relying on host-specific paths like /etc/passwd).
  let tmp = tempfile::tempdir().expect("tempdir");
  let probe_path = tmp.path().join("probe.txt");
  std::fs::write(&probe_path, b"probe").expect("write probe file");

  let mut sandbox_config = fastrender::sandbox::RendererSandboxConfig::default();
  // Apply Landlock when supported as additional defense-in-depth. This is best-effort: kernels that
  // don't support Landlock will still run the seccomp sandbox.
  sandbox_config.landlock = fastrender::sandbox::RendererLandlockPolicy::RestrictWrites;
  let sandbox_status = fastrender::sandbox::apply_renderer_sandbox(sandbox_config);
  match sandbox_status {
    Ok(
      fastrender::sandbox::SandboxStatus::Applied
      | fastrender::sandbox::SandboxStatus::AppliedWithoutTsync,
    ) => {}
    Ok(
      fastrender::sandbox::SandboxStatus::DisabledByEnv
      | fastrender::sandbox::SandboxStatus::DisabledByConfig
      | fastrender::sandbox::SandboxStatus::ReportOnly
      | fastrender::sandbox::SandboxStatus::Unsupported,
    ) => {
      eprintln!("skipping: renderer sandbox disabled/unsupported");
      return;
    }
    Err(err) => {
      // Skip gracefully on kernels that don't support seccomp/required flags (or environments that
      // reject sandbox installation). This keeps the test usable across a wide range of Linux CI
      // hosts while still exercising the sandbox where possible.
      let errno = match &err {
        fastrender::sandbox::SandboxError::SetParentDeathSignalFailed { source } => {
          source.raw_os_error()
        }
        fastrender::sandbox::SandboxError::SetDumpableFailed { source }
        | fastrender::sandbox::SandboxError::DisableCoreDumpsFailed { source }
        | fastrender::sandbox::SandboxError::EnableNoNewPrivsFailed { source } => source.raw_os_error(),
        fastrender::sandbox::SandboxError::LandlockFailed { source } => match source {
          fastrender::sandbox::linux_landlock::LandlockError::ProbeFailed { source }
          | fastrender::sandbox::linux_landlock::LandlockError::CreateRulesetFailed { source }
          | fastrender::sandbox::linux_landlock::LandlockError::OpenPathFailed { source, .. }
          | fastrender::sandbox::linux_landlock::LandlockError::AddRuleFailed { source, .. }
          | fastrender::sandbox::linux_landlock::LandlockError::SetNoNewPrivsFailed { source }
          | fastrender::sandbox::linux_landlock::LandlockError::RestrictSelfFailed { source } => {
            source.raw_os_error()
          }
        },
        fastrender::sandbox::SandboxError::SeccompInstallRejected { errno, .. } => Some(*errno),
        fastrender::sandbox::SandboxError::SeccompInstallFailed { errno, .. } => Some(*errno),
        _ => None,
      };
      if matches!(errno, Some(code) if code == libc::ENOSYS || code == libc::EINVAL || code == libc::EPERM) {
        eprintln!("skipping: renderer sandbox not supported/allowed (errno={errno:?}, err={err})");
        return;
      }
      panic!("apply renderer sandbox failed: {err}");
    }
  }

  // Exercise a small amount of threading + timing after sandboxing. This is not the primary goal
  // of the test (rendering is), but it provides additional coverage for denylist regressions.
  let start = std::time::Instant::now();
  std::thread::sleep(Duration::from_millis(1));
  assert!(
    start.elapsed() < Duration::from_secs(5),
    "expected sleep/clock syscalls to keep working under sandbox"
  );
  // Also assert the syscall filter applies to newly-spawned threads (important for TSYNC fallback
  // kernels where the filter must be installed before spawning any threads).
  let probe_path_bytes = probe_path.as_os_str().as_bytes().to_vec();
  let (open_errno, socket_errno) = std::thread::Builder::new()
    .name("fastr-sandbox-smoke-thread".to_string())
    .spawn(move || {
      use std::ffi::CString;
      use std::io;

      let probe_cstr = CString::new(probe_path_bytes).expect("cstr probe path bytes");
      let fd = unsafe { libc::open(probe_cstr.as_ptr(), libc::O_RDONLY) };
      let open_errno = if fd == -1 {
        io::Error::last_os_error().raw_os_error()
      } else {
        unsafe { libc::close(fd) };
        Some(0)
      };

      let sock = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
      let socket_errno = if sock == -1 {
        io::Error::last_os_error().raw_os_error()
      } else {
        unsafe { libc::close(sock) };
        Some(0)
      };

      (open_errno, socket_errno)
    })
    .expect("spawn thread under sandbox")
    .join()
    .expect("join thread under sandbox");
  assert_eq!(
    open_errno,
    Some(libc::EPERM),
    "expected open() to be blocked in sandboxed thread"
  );
  assert_eq!(
    socket_errno,
    Some(libc::EPERM),
    "expected socket() to be blocked in sandboxed thread"
  );

  // Minimal offline HTML render using only bundled fonts (avoids system font scanning).
  let mut paint = fastrender::PaintParallelism::enabled();
  // Keep the tile size small so even tiny viewports exercise the parallel paint pipeline.
  paint.tile_size = 8;
  paint.min_display_items = 0;
  paint.min_tiles = 0;

  let config = fastrender::FastRenderConfig::default()
    .with_font_sources(fastrender::FontConfig::bundled_only())
    .with_paint_parallelism(paint);

  let mut renderer = fastrender::FastRender::with_config(config).expect("init renderer");
  let html = r#"<!doctype html>
<html>
  <head>
    <style>
      html, body { margin: 0; padding: 0; background: #fff; }
      body { font: 12px "Roboto Flex", sans-serif; }
      .box { width: 16px; height: 16px; background: rgb(255, 0, 0); }
    </style>
  </head>
  <body>
    <div class="box"></div>
    <div>Hello</div>
  </body>
</html>"#;

  let pixmap = renderer
    .render_html(html, 64, 64)
    .expect("render minimal HTML under sandbox");
  assert_eq!(pixmap.width(), 64);
  assert_eq!(pixmap.height(), 64);

  // After sandboxing, filesystem operations should be blocked.
  let probe_cstr = CString::new(probe_path.as_os_str().as_bytes()).expect("cstr probe path");
  let fd = unsafe { libc::open(probe_cstr.as_ptr(), libc::O_RDONLY) };
  assert_eq!(
    fd, -1,
    "expected open() to fail under sandbox (path={})",
    probe_path.display()
  );
  let open_err = io::Error::last_os_error();
  assert_eq!(
    open_err.raw_os_error(),
    Some(libc::EPERM),
    "expected open() to be blocked by seccomp (errno={open_err:?})"
  );

  // Network operations should also be blocked.
  let sock = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
  assert_eq!(sock, -1, "expected socket() to fail under sandbox");
  let sock_err = io::Error::last_os_error();
  assert_eq!(
    sock_err.raw_os_error(),
    Some(libc::EPERM),
    "expected socket() to be blocked by seccomp (errno={sock_err:?})"
  );
}

#[cfg(not(target_os = "linux"))]
#[test]
fn sandboxed_minimal_offline_render_succeeds() {
  // Sandbox regression tests are Linux-only (seccomp/landlock).
}
