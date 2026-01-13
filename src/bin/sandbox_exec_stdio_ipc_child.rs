//! Helper binary for macOS sandbox-exec integration tests.
//!
//! The corresponding integration test spawns this binary through `/usr/bin/sandbox-exec` and
//! validates that stdio pipes remain usable for IPC.

use std::io::{self, BufRead, Write};

const ENV_STDIN_SENTINEL: &str = "FASTR_TEST_SANDBOX_EXEC_STDIO_IPC_STDIN_SENTINEL";
const ENV_STDOUT_SENTINEL: &str = "FASTR_TEST_SANDBOX_EXEC_STDIO_IPC_STDOUT_SENTINEL";

fn main() {
  let stdin_sentinel =
    std::env::var(ENV_STDIN_SENTINEL).unwrap_or_else(|_| "fastrender-stdio-ipc-in".to_string());
  let stdout_sentinel =
    std::env::var(ENV_STDOUT_SENTINEL).unwrap_or_else(|_| "fastrender-stdio-ipc-out".to_string());

  let mut line = String::new();
  let mut stdin = io::stdin().lock();
  stdin
    .read_line(&mut line)
    .expect("read line from stdin"); // fastrender-allow-unwrap
  let received = line.trim_end_matches(&['\r', '\n'][..]);
  if received != stdin_sentinel {
    eprintln!(
      "stdin sentinel mismatch: expected {stdin_sentinel:?}, got {received:?}"
    );
    std::process::exit(2);
  }

  let mut stdout = io::stdout().lock();
  writeln!(stdout, "{stdout_sentinel}").expect("write stdout sentinel"); // fastrender-allow-unwrap
  stdout.flush().expect("flush stdout"); // fastrender-allow-unwrap
}
