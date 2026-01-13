//! Linux-only sandbox smoke test that performs a real render after applying seccomp.
//!
//! This is a regression test for the renderer's "no ambient filesystem/network access" goal.
//! It intentionally installs a seccomp filter that denies:
//! - `open*()`/`creat()` (filesystem reads/writes) via `EPERM`
//! - `socket()`/`socketpair()` (network access) via `EPERM`
//!
//! The render pipeline must therefore complete successfully even when the OS refuses any ambient
//! filesystem or network operations.
//!
//! The test runs the sandboxed portion in a child process to avoid affecting the wider test
//! harness (seccomp is process-wide and cannot be reverted).

#![cfg(target_os = "linux")]

use fastrender::resource::{FetchedResource, ResourceFetcher};
use fastrender::{FastRender, FontConfig, ResourcePolicy, Rgba, Result};
use std::io;
use std::process::Command;
use std::sync::Arc;

const CHILD_ENV: &str = "FASTR_TEST_SANDBOX_SMOKE_RENDER_CHILD";

#[test]
fn sandbox_smoke_render_completes_after_sandboxing() {
  let is_child = std::env::var_os(CHILD_ENV).is_some();
  if is_child {
    // Keep this render hermetic/deterministic even if other subsystems consult the default
    // FontConfig (e.g. SVG helpers).
    apply_renderer_sandbox().expect("apply renderer sandbox");

    // Sanity check that the sandbox is active (and that we're not accidentally running without
    // seccomp).
    assert_sandbox_denies_open();
    assert_sandbox_denies_socket();

    let viewport = (128_u32, 64_u32);
    let policy = ResourcePolicy::default()
      .allow_http(false)
      .allow_https(false)
      .allow_file(false)
      // Keep `data:` enabled so the pipeline can still support inline resources.
      .allow_data(true);

    let mut renderer = FastRender::builder()
      .viewport_size(viewport.0, viewport.1)
      // Start transparent so we can assert paint actually ran by checking background pixels.
      .background_color(Rgba::TRANSPARENT)
      .font_sources(FontConfig::bundled_only())
      .resource_policy(policy)
      // Avoid constructing the default HTTP fetcher (which can spin up background threads and try
      // to create sockets) — this smoke test intentionally runs with no subresource fetches.
      .fetcher(Arc::new(PanicFetcher))
      .build()
      .expect("create renderer with bundled fonts");

    let html = r#"<!DOCTYPE html>
<html>
  <head>
    <style>
      html, body { margin: 0; background: #123456; }
      body { padding: 12px; font-family: sans-serif; }
    </style>
  </head>
  <body>
    <div>Hello</div>
  </body>
</html>
"#;

    let pixmap = renderer
      .render_html(html, viewport.0, viewport.1)
      .expect("render should succeed under sandbox");

    assert_eq!(pixmap.width(), viewport.0, "pixmap width mismatch");
    assert_eq!(pixmap.height(), viewport.1, "pixmap height mismatch");

    // Background should have been painted (i.e. not left transparent).
    let px = pixmap
      .pixel(1, 1)
      .expect("expected pixel within bounds");
    assert_eq!(
      (px.red(), px.green(), px.blue(), px.alpha()),
      (0x12, 0x34, 0x56, 0xFF),
      "expected painted background pixel"
    );

    return;
  }

  // Parent process: spawn a child copy of this test binary so seccomp does not affect the rest of
  // the test suite (seccomp filters are irreversible and process-wide).
  let exe = std::env::current_exe().expect("current test exe path");
  let test_name = "sandbox_smoke_render_completes_after_sandboxing";
  let output = Command::new(exe)
    .env(CHILD_ENV, "1")
    // Keep the child process's Rayon global thread pool small so CI isn't stressed while sandboxed.
    .env("RAYON_NUM_THREADS", "1")
    .arg("--exact")
    .arg(test_name)
    .arg("--nocapture")
    .output()
    .expect("spawn child test process");

  assert!(
    output.status.success(),
    "child process should exit successfully (stdout={}, stderr={})",
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
}

fn apply_renderer_sandbox() -> io::Result<()> {
  // Unprivileged seccomp filters require `no_new_privs`.
  // SAFETY: `prctl` is a Linux syscall. We pass the expected argument types.
  let rc = unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) };
  if rc != 0 {
    return Err(io::Error::last_os_error());
  }

  apply_seccomp_filter()
}

#[derive(Debug)]
struct PanicFetcher;

impl ResourceFetcher for PanicFetcher {
  fn fetch(&self, url: &str) -> Result<FetchedResource> {
    panic!("sandbox smoke render attempted to fetch a resource: {url}");
  }
}

fn apply_seccomp_filter() -> io::Result<()> {
  // Minimal BPF filter:
  // - Allow everything by default.
  // - Return EPERM for syscalls that provide ambient filesystem/network access.

  // seccomp return values (from linux/seccomp.h)
  const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
  const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
  const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;

  // BPF constants (from linux/filter.h)
  const BPF_LD: u16 = 0x00;
  const BPF_W: u16 = 0x00;
  const BPF_ABS: u16 = 0x20;
  const BPF_JMP: u16 = 0x05;
  const BPF_JEQ: u16 = 0x10;
  const BPF_K: u16 = 0x00;
  const BPF_RET: u16 = 0x06;

  const SECCOMP_DATA_NR_OFFSET: u32 = 0;
  const SECCOMP_DATA_ARCH_OFFSET: u32 = 4;

  fn stmt(code: u16, k: u32) -> libc::sock_filter {
    libc::sock_filter {
      code,
      jt: 0,
      jf: 0,
      k,
    }
  }

  fn jump(code: u16, k: u32, jt: u8, jf: u8) -> libc::sock_filter {
    libc::sock_filter { code, jt, jf, k }
  }

  let mut filter: Vec<libc::sock_filter> = Vec::new();

  let deny_action = SECCOMP_RET_ERRNO | (libc::EPERM as u32);

  // Best-effort arch check to avoid accidentally interpreting syscall numbers with the wrong ABI.
  // libc does not currently expose the AUDIT_ARCH_* constants, so define the commonly-used ones
  // directly (from linux/audit.h).
  #[cfg(target_arch = "x86_64")]
  let audit_arch: Option<u32> = Some(0xC000_003E);
  #[cfg(target_arch = "aarch64")]
  let audit_arch: Option<u32> = Some(0xC000_00B7);
  #[cfg(target_arch = "arm")]
  let audit_arch: Option<u32> = Some(0x4000_0028);
  #[cfg(target_arch = "x86")]
  let audit_arch: Option<u32> = Some(0x4000_0003);
  #[cfg(target_arch = "riscv64")]
  let audit_arch: Option<u32> = Some(0xC000_00F3);
  #[cfg(not(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "arm",
    target_arch = "x86",
    target_arch = "riscv64"
  )))]
  let audit_arch: Option<u32> = None;

  if let Some(expected_arch) = audit_arch {
    filter.push(stmt(BPF_LD | BPF_W | BPF_ABS, SECCOMP_DATA_ARCH_OFFSET));
    // If arch == expected, skip the kill instruction.
    filter.push(jump(
      BPF_JMP | BPF_JEQ | BPF_K,
      expected_arch,
      1,
      0,
    ));
    // If the arch is not what we expect, default to killing the process rather than interpreting
    // syscall numbers with the wrong ABI.
    filter.push(stmt(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS));
  }

  filter.push(stmt(BPF_LD | BPF_W | BPF_ABS, SECCOMP_DATA_NR_OFFSET));

  // Syscalls we want to forbid in the sandbox.
  //
  // Note: keep this list minimal and focused on "ambient authority" operations. The goal is to
  // catch accidental filesystem/network dependencies in the render pipeline.
  let forbidden_syscalls: &[u32] = &[
    libc::SYS_open as u32,
    libc::SYS_openat as u32,
    libc::SYS_openat2 as u32,
    libc::SYS_creat as u32,
    libc::SYS_socket as u32,
    libc::SYS_socketpair as u32,
  ];
  if std::env::var_os("FASTR_TEST_SANDBOX_SMOKE_DEBUG").is_some() {
    eprintln!("seccomp forbidden syscalls: {forbidden_syscalls:?}");
  }

  for &nr in forbidden_syscalls {
    // If syscall == nr, deny it; otherwise continue.
    filter.push(jump(BPF_JMP | BPF_JEQ | BPF_K, nr, 0, 1));
    filter.push(stmt(BPF_RET | BPF_K, deny_action));
  }

  filter.push(stmt(BPF_RET | BPF_K, SECCOMP_RET_ALLOW));

  let prog = libc::sock_fprog {
    len: filter
      .len()
      .try_into()
      .map_err(|_| io::Error::new(io::ErrorKind::Other, "seccomp filter too large"))?,
    filter: filter.as_ptr() as *mut libc::sock_filter,
  };

  // SAFETY: `prctl` is a Linux syscall. `prog` and `filter` live long enough for the call.
  let rc = unsafe { libc::prctl(libc::PR_SET_SECCOMP, libc::SECCOMP_MODE_FILTER, &prog) };
  if rc != 0 {
    return Err(io::Error::last_os_error());
  }
  Ok(())
}

fn assert_sandbox_denies_open() {
  match std::fs::read_to_string("/proc/self/cgroup") {
    Ok(_) => panic!("expected openat(/proc/self/cgroup) to be denied by seccomp sandbox"),
    Err(err) => {
      assert_eq!(
        err.kind(),
        io::ErrorKind::PermissionDenied,
        "expected EPERM from blocked openat()"
      );
    }
  }
}

fn assert_sandbox_denies_socket() {
  // SAFETY: `socket` is a libc syscall wrapper; we expect it to fail with EPERM under seccomp.
  let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
  if fd >= 0 {
    // SAFETY: `close` is safe to call with a valid fd; this shouldn't happen but avoid leaking.
    unsafe {
      libc::close(fd);
    }
    panic!("expected socket(AF_INET, SOCK_STREAM) to be denied by seccomp sandbox");
  }
  let err = io::Error::last_os_error();
  assert_eq!(
    err.raw_os_error(),
    Some(libc::EPERM),
    "expected EPERM from blocked socket()"
  );
}
