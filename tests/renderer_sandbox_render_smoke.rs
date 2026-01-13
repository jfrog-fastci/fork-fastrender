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

  let sandbox_status =
    fastrender::sandbox::apply_renderer_sandbox(fastrender::sandbox::RendererSandboxConfig::default());
  match sandbox_status {
    Ok(fastrender::sandbox::SandboxStatus::Applied) => {}
    Ok(fastrender::sandbox::SandboxStatus::Disabled | fastrender::sandbox::SandboxStatus::Unsupported) => {
      eprintln!("skipping: renderer sandbox unsupported on this platform");
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
          fastrender::sandbox::linux_landlock::LandlockError::SetNoNewPrivsFailed { source } => {
            source.raw_os_error()
          }
          _ => None,
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
  std::thread::Builder::new()
    .name("fastr-sandbox-smoke-thread".to_string())
    .spawn(|| 1u8)
    .expect("spawn thread under sandbox")
    .join()
    .expect("join thread under sandbox");

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
