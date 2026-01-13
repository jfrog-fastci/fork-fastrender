use std::io::{self, Read};
use std::process::ExitCode;
use std::time::Duration;

fn main() -> ExitCode {
  let mut args = std::env::args().skip(1);
  let mode = args.next().unwrap_or_default();

  match mode.as_str() {
    "sleep" => {
      // Block until the parent lets us proceed. This allows the test
      // harness to assign us to a job before we do anything interesting.
      wait_for_parent_signal();
      std::thread::sleep(Duration::from_secs(60));
      ExitCode::SUCCESS
    }
    "spawn-grandchild" => {
      wait_for_parent_signal();

      let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
          eprintln!("current_exe failed: {e}");
          return ExitCode::from(2);
        }
      };

      // Attempt to spawn another process. If the parent configured the
      // job with `ActiveProcessLimit=1`, this should fail.
      let spawn_result = std::process::Command::new(exe)
        .arg("sleep")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();

      match spawn_result {
        Ok(mut child) => {
          // Unexpected: job limit did not block. Try to clean up.
          let _ = child.kill();
          let _ = child.wait();
          ExitCode::from(1)
        }
        Err(_e) => ExitCode::SUCCESS,
      }
    }
    _ => {
      eprintln!("usage: job_test_helper <sleep|spawn-grandchild>");
      ExitCode::from(64)
    }
  }
}

fn wait_for_parent_signal() {
  // Read until EOF or newline. The parent holds the write end; this lets us
  // block without any platform-specific primitives.
  let mut buf = [0u8; 1];
  let mut stdin = io::stdin();
  let _ = stdin.read(&mut buf);
}
