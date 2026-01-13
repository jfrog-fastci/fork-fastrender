#[cfg(target_os = "linux")]
mod linux {
  use std::ffi::CString;
  use std::os::unix::ffi::OsStrExt;
  use std::process::Command;

  fn is_seccomp_unsupported_error(err: &fastrender::sandbox::SandboxError) -> bool {
    let errno = match err {
      fastrender::sandbox::SandboxError::SetDumpableFailed { source }
      | fastrender::sandbox::SandboxError::DisableCoreDumpsFailed { source }
      | fastrender::sandbox::SandboxError::EnableNoNewPrivsFailed { source } => source.raw_os_error(),
      fastrender::sandbox::SandboxError::SeccompInstallRejected { errno, .. } => Some(*errno),
      fastrender::sandbox::SandboxError::SeccompInstallFailed { errno, .. } => Some(*errno),
      _ => None,
    };
    matches!(errno, Some(code) if code == libc::ENOSYS || code == libc::EINVAL)
  }

  #[test]
  fn seccomp_denies_filesystem_mutation_syscalls() {
    const CHILD_ENV: &str = "FASTR_TEST_SECCOMP_FS_MUT_CHILD";
    const FILE_ENV: &str = "FASTR_TEST_SECCOMP_FS_MUT_FILE";
    const DIR_ENV: &str = "FASTR_TEST_SECCOMP_FS_MUT_DIR";

    let is_child = std::env::var_os(CHILD_ENV).is_some();
    if is_child {
      let file_path = std::env::var_os(FILE_ENV).expect("child must receive file path");
      let dir_path = std::env::var_os(DIR_ENV).expect("child must receive dir path");

      let file_cstr =
        CString::new(file_path.as_os_str().as_bytes()).expect("file path must be a C string");
      let dir_cstr =
        CString::new(dir_path.as_os_str().as_bytes()).expect("dir path must be a C string");

      match fastrender::sandbox::apply_renderer_seccomp_denylist() {
        Ok(fastrender::sandbox::SandboxStatus::Applied)
        | Ok(fastrender::sandbox::SandboxStatus::AppliedWithoutTsync) => {}
        Ok(fastrender::sandbox::SandboxStatus::Disabled | fastrender::sandbox::SandboxStatus::Unsupported) => return,
        Err(err) => {
          if is_seccomp_unsupported_error(&err) {
            return;
          }
          panic!("failed to apply seccomp sandbox in child: {err}");
        }
      }

      // Attempt to remove the file - should be blocked by seccomp and return EPERM (not ENOENT).
      let rc = unsafe { libc::unlink(file_cstr.as_ptr()) };
      assert_eq!(rc, -1, "unlink should fail under seccomp");
      let err = std::io::Error::last_os_error();
      assert_eq!(
        err.raw_os_error(),
        Some(libc::EPERM),
        "expected unlink to be denied with EPERM (got {err:?})"
      );

      // Attempt to create a directory - should also be blocked and return EPERM.
      let rc = unsafe { libc::mkdir(dir_cstr.as_ptr(), 0o700) };
      assert_eq!(rc, -1, "mkdir should fail under seccomp");
      let err = std::io::Error::last_os_error();
      assert_eq!(
        err.raw_os_error(),
        Some(libc::EPERM),
        "expected mkdir to be denied with EPERM (got {err:?})"
      );

      return;
    }

    let tmp_file = tempfile::NamedTempFile::new().expect("create temp file");
    let tmp_dir = tempfile::tempdir().expect("create temp dir");
    let mkdir_path = tmp_dir.path().join("seccomp_mkdir_denied");

    let exe = std::env::current_exe().expect("current test exe path");
    let test_name = "seccomp_denies_filesystem_mutation_syscalls";
    let output = Command::new(exe)
      .env(CHILD_ENV, "1")
      // Avoid a large libtest threadpool: the sandbox is process-global. When TSYNC is supported it
      // applies to all threads; when TSYNC is unavailable the sandbox must be installed before any
      // additional threads spawn.
      .env("RUST_TEST_THREADS", "1")
      .env(FILE_ENV, tmp_file.path())
      .env(DIR_ENV, &mkdir_path)
      .arg("--exact")
      .arg(test_name)
      .arg("--nocapture")
      .output()
      .expect("spawn seccomp child test process");

    assert!(
      output.status.success(),
      "child process should exit successfully (stdout={}, stderr={})",
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr)
    );
  }
}
