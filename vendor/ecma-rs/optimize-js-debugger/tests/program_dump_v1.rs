use optimize_js_debugger::ProgramDump;
use std::path::PathBuf;
use std::process::Command;
 
#[test]
fn snapshot_generator_emits_program_dump_v1_json() {
  let exe = env!("CARGO_BIN_EXE_optimize-js-debugger");
  let fixture = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/debug_input.js");
 
  let output = Command::new(exe)
    .arg("--snapshot")
    .arg("--input")
    .arg(&fixture)
    .output()
    .expect("run optimize-js-debugger --snapshot");
 
  assert!(
    output.status.success(),
    "snapshot generator failed: {}\nstdout:\n{}\nstderr:\n{}",
    output.status,
    String::from_utf8_lossy(&output.stdout),
    String::from_utf8_lossy(&output.stderr)
  );
 
  let parsed: ProgramDump =
    serde_json::from_slice(&output.stdout).expect("snapshot output should be valid ProgramDump JSON");
  let _v1 = parsed.into_v1();
}

