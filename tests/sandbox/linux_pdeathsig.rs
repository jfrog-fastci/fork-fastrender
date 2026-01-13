#![cfg(target_os = "linux")]

use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const ROLE_ENV: &str = "FASTR_TEST_PDEATHSIG_ROLE";
const ROLE_PARENT: &str = "parent";
const ROLE_CHILD: &str = "child";

const TEST_NAME: &str = concat!(module_path!(), "::sandbox_linux_pdeathsig_kills_orphaned_child");

const CHILD_PID_PREFIX: &str = "FASTR_TEST_PDEATHSIG_CHILD_PID=";
const READY_MARKER: &str = "FASTR_TEST_PDEATHSIG_READY";

const READY_TIMEOUT: Duration = Duration::from_secs(2);
const PARENT_EXIT_TIMEOUT: Duration = Duration::from_secs(4);
const CHILD_EXIT_TIMEOUT: Duration = Duration::from_secs(2);

fn set_child_subreaper() {
  // Ensure orphaned grandchildren are reparented back to this test process so we can `waitpid` and
  // inspect the terminating signal.
  //
  // SAFETY: `prctl` is a process-global syscall. We supply valid arguments and check for errors.
  let rc = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
  assert_eq!(
    rc,
    0,
    "PR_SET_CHILD_SUBREAPER failed: {}",
    std::io::Error::last_os_error()
  );
}

fn wait_for_exit_with_timeout(
  child: &mut std::process::Child,
  timeout: Duration,
) -> std::process::ExitStatus {
  let start = Instant::now();
  loop {
    if let Some(status) = child.try_wait().expect("try_wait") {
      return status;
    }
    if start.elapsed() > timeout {
      let _ = child.kill();
      let _ = child.wait();
      panic!("timed out waiting for helper process to exit after {timeout:?}");
    }
    std::thread::sleep(Duration::from_millis(10));
  }
}

fn parse_child_pid_line(line: &str) -> Option<libc::pid_t> {
  let value = line.strip_prefix(CHILD_PID_PREFIX)?.trim();
  value.parse::<libc::pid_t>().ok()
}

fn decode_wait_status(status: libc::c_int) -> Result<(), String> {
  // Based on POSIX wait status encoding.
  let low = status & 0x7f;
  if low == 0 {
    let code = (status >> 8) & 0xff;
    return Err(format!("child exited normally with code {code} (expected SIGKILL)"));
  }
  if low == 0x7f {
    return Err(format!(
      "child was stopped/continued unexpectedly (raw wait status={status})"
    ));
  }

  let signal = low;
  if signal != libc::SIGKILL {
    return Err(format!(
      "child terminated by signal {signal} (expected {})",
      libc::SIGKILL
    ));
  }
  Ok(())
}

fn controller_main(test_name: &str) {
  set_child_subreaper();

  let exe = std::env::current_exe().expect("current test exe path");

  let mut parent = Command::new(exe)
    .env(ROLE_ENV, ROLE_PARENT)
    .arg("--exact")
    .arg(test_name)
    .arg("--nocapture")
    .stdout(Stdio::piped())
    .stderr(Stdio::piped())
    .spawn()
    .expect("spawn PDEATHSIG helper parent");

  let parent_stdout = parent.stdout.take().expect("parent stdout pipe");
  let parent_stderr = parent.stderr.take().expect("parent stderr pipe");

  let (pid_tx, pid_rx) = std::sync::mpsc::channel::<libc::pid_t>();
  let stdout_join = std::thread::spawn(move || {
    let mut buf = String::new();
    let mut reader = BufReader::new(parent_stdout);
    let mut line = String::new();
    let mut sent_pid = false;
    loop {
      line.clear();
      let n = reader.read_line(&mut line).unwrap_or(0);
      if n == 0 {
        break;
      }
      if !sent_pid {
        if let Some(pid) = parse_child_pid_line(&line) {
          sent_pid = true;
          let _ = pid_tx.send(pid);
        }
      }
      buf.push_str(&line);
    }
    buf
  });

  let stderr_join = std::thread::spawn(move || {
    let mut buf = String::new();
    let mut reader = BufReader::new(parent_stderr);
    let _ = reader.read_to_string(&mut buf);
    buf
  });

  let pid = pid_rx
    .recv_timeout(READY_TIMEOUT)
    .unwrap_or_else(|_| panic!("timed out waiting for helper parent to print child PID"));

  let parent_status = wait_for_exit_with_timeout(&mut parent, PARENT_EXIT_TIMEOUT);

  let parent_stdout = stdout_join.join().unwrap_or_default();
  let parent_stderr = stderr_join.join().unwrap_or_default();

  assert!(
    parent_status.success(),
    "helper parent exited with {parent_status} (stdout={parent_stdout:?}, stderr={parent_stderr:?})"
  );

  let deadline = Instant::now() + CHILD_EXIT_TIMEOUT;
  let mut status: libc::c_int = 0;
  loop {
    // SAFETY: `waitpid` writes to `status` and uses a pid we obtained from our own child process.
    let rc = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
    if rc == pid {
      break;
    }
    if rc == 0 {
      if Instant::now() >= deadline {
        // Best-effort cleanup: if the child is still alive, kill it so the test binary doesn't leak
        // processes in CI.
        unsafe {
          libc::kill(pid, libc::SIGKILL);
          libc::waitpid(pid, &mut status, 0);
        }
        panic!("timed out waiting for orphaned child to be killed by PDEATHSIG");
      }
      std::thread::sleep(Duration::from_millis(10));
      continue;
    }

    let err = std::io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ECHILD) {
      if Instant::now() >= deadline {
        panic!(
          "waitpid returned ECHILD for PID {pid} (subreaper/reparenting failed?) and timeout elapsed"
        );
      }
      std::thread::sleep(Duration::from_millis(10));
      continue;
    }
    panic!("waitpid({pid}) failed: {err}");
  }

  if let Err(err) = decode_wait_status(status) {
    panic!("{err}");
  }
}

fn parent_main(test_name: &str) {
  let exe = std::env::current_exe().expect("current test exe path");
  let mut child = Command::new(exe)
    .env(ROLE_ENV, ROLE_CHILD)
    .arg("--exact")
    .arg(test_name)
    .arg("--nocapture")
    .stdout(Stdio::null())
    .stderr(Stdio::piped())
    .spawn()
    .expect("spawn PDEATHSIG child");

  let child_pid = libc::pid_t::try_from(child.id())
    .unwrap_or_else(|_| panic!("child pid {} did not fit pid_t", child.id()));

  println!("{CHILD_PID_PREFIX}{child_pid}");
  std::io::stdout().flush().ok();

  let child_stderr = child.stderr.take().expect("child stderr pipe");
  let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
  let stderr_join = std::thread::spawn(move || {
    let mut buf = String::new();
    let mut reader = BufReader::new(child_stderr);
    let mut line = String::new();
    loop {
      line.clear();
      let n = reader.read_line(&mut line).unwrap_or(0);
      if n == 0 {
        break;
      }
      if line.contains(READY_MARKER) {
        let _ = ready_tx.send(());
        buf.push_str(&line);
        break;
      }
      buf.push_str(&line);
    }
    buf
  });

  if ready_rx.recv_timeout(READY_TIMEOUT).is_err() {
    let _ = child.kill();
    let _ = child.wait();
    let stderr = stderr_join.join().unwrap_or_default();
    panic!("child did not signal readiness within {READY_TIMEOUT:?} (stderr={stderr:?})");
  }
  let _ = stderr_join.join();

  // Exit quickly without waiting/reaping the child. With PDEATHSIG configured, the child should be
  // killed when this process terminates.
  std::process::exit(0);
}

fn child_main() {
  fastrender::sandbox::linux_set_parent_death_signal()
    .expect("linux_set_parent_death_signal should succeed");

  eprintln!("{READY_MARKER}");
  std::io::stderr().flush().ok();

  loop {
    std::thread::sleep(Duration::from_secs(60));
  }
}

#[test]
fn sandbox_linux_pdeathsig_kills_orphaned_child() {
  match std::env::var(ROLE_ENV).ok().as_deref() {
    Some(ROLE_PARENT) => parent_main(TEST_NAME),
    Some(ROLE_CHILD) => child_main(),
    Some(other) => panic!("unknown {ROLE_ENV} value {other:?}"),
    None => controller_main(TEST_NAME),
  }
}
