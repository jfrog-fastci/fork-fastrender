use std::path::PathBuf;
use std::process::{Command, Stdio};

fn tool_available(tool: &str) -> bool {
  Command::new(tool)
    .arg("--version")
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status()
    .is_ok()
}

fn run_success(mut cmd: Command) -> String {
  let cmd_str = format!("{cmd:?}");
  let out = cmd
    .output()
    .unwrap_or_else(|e| panic!("failed to run {cmd_str}: {e}"));
  if !out.status.success() {
    panic!(
      "command failed: {cmd_str}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
      out.status,
      String::from_utf8_lossy(&out.stdout),
      String::from_utf8_lossy(&out.stderr),
    );
  }
  String::from_utf8(out.stdout).expect("command output must be UTF-8")
}

fn parse_decimal_after_prefix(s: &str, prefix: &str) -> Option<u64> {
  s.lines().find_map(|line| {
    let line = line.trim_start();
    let rest = line.strip_prefix(prefix)?;
    let num = rest.trim().split_whitespace().next()?;
    num.parse::<u64>().ok()
  })
}

fn parse_stackmap_first_instruction_offset(stackmap: &str) -> Option<u64> {
  stackmap.lines().find_map(|line| {
    let line = line.trim_start();
    let (_, rest) = line.split_once("instruction offset: ")?;
    rest.trim().split_whitespace().next()?.parse::<u64>().ok()
  })
}

fn stackmap_first_record_has_non_constant_location(stackmap: &str) -> bool {
  let mut in_first_record = false;
  let mut in_locations = false;

  for line in stackmap.lines() {
    let line = line.trim_start();

    if line.starts_with("Record ID: ") {
      if in_first_record {
        break;
      }
      in_first_record = true;
      continue;
    }
    if !in_first_record {
      continue;
    }

    if line.ends_with("locations:") {
      in_locations = true;
      continue;
    }

    if !in_locations {
      continue;
    }

    if line.contains("live-outs:") {
      break;
    }

    // Location lines look like:
    //   #4: Indirect [R#7 + 0], size: 8
    if line.starts_with('#') && !line.contains("Constant") {
      return true;
    }
  }

  false
}

fn parse_call_return_offset_from_objdump(objdump: &str, fn_name: &str) -> Option<u64> {
  let mut in_fn = false;
  let mut fn_start = 0u64;
  let mut saw_call = false;

  for line in objdump.lines() {
    let line_trimmed = line.trim_end();

    // Function header lines look like:
    //   0000000000000000 <test>:
    if line_trimmed.ends_with(&format!("<{fn_name}>:")) {
      let addr_str = line_trimmed.split_whitespace().next()?;
      fn_start = u64::from_str_radix(addr_str, 16).ok()?;
      in_fn = true;
      saw_call = false;
      continue;
    }

    if !in_fn {
      continue;
    }

    // Stop if we hit the next function header.
    if line_trimmed.contains('<') && line_trimmed.ends_with(">:") {
      break;
    }

    // Instruction lines look like:
    //        5:       callq ...
    let inst_line = line_trimmed.trim_start();
    let (addr_str, rest) = inst_line.split_once(':')?;
    let addr = u64::from_str_radix(addr_str.trim(), 16).ok()?;

    // If the previous instruction was a call, this address is the return address.
    if saw_call {
      return Some(addr - fn_start);
    }

    let inst = rest.trim_start();
    if inst.starts_with("call") {
      saw_call = true;
    }
  }

  None
}

pub(crate) fn llvm18_statepoint_fixture_emits_verified_stackmaps() {
  // This is an LLVM18-specific test; if the tools are missing, treat as a skip.
  for tool in ["llvm-as-18", "llc-18", "llvm-readobj-18", "llvm-objdump-18"] {
    if !tool_available(tool) {
      eprintln!("skipping: {tool} not available in PATH");
      return;
    }
  }

  let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let fixture = manifest_dir.join("tests/fixtures/llvm/statepoint_min.ll");
  assert!(fixture.exists(), "missing fixture: {fixture:?}");

  let tmp = tempfile::tempdir().expect("create tempdir");
  let bc = tmp.path().join("statepoint_min.bc");
  let obj = tmp.path().join("statepoint_min.o");

  // 1) Assemble with verification enabled (default for llvm-as).
  let mut cmd = Command::new("llvm-as-18");
  cmd.arg(&fixture).arg("-o").arg(&bc);
  run_success(cmd);

  // 2) Compile to an object with stackmaps.
  let mut cmd = Command::new("llc-18");
  cmd.arg("-filetype=obj").arg(&bc).arg("-o").arg(&obj);
  run_success(cmd);

  // 3) Assert `.llvm_stackmaps` exists and has at least one record.
  let mut cmd = Command::new("llvm-readobj-18");
  cmd.arg("--stackmap").arg(&obj);
  let stackmap = run_success(cmd);
  let version = parse_decimal_after_prefix(&stackmap, "LLVM StackMap Version: ")
    .expect("could not parse LLVM StackMap Version");
  assert_eq!(version, 3, "unexpected stackmap version:\n{stackmap}");

  let num_records =
    parse_decimal_after_prefix(&stackmap, "Num Records: ").expect("could not parse Num Records");
  assert!(num_records >= 1, "expected >= 1 record:\n{stackmap}");

  let record_inst_offset = parse_stackmap_first_instruction_offset(&stackmap)
    .expect("could not find stackmap record instruction offset");

  assert!(
    stackmap_first_record_has_non_constant_location(&stackmap),
    "expected stackmap record to contain at least one non-constant location (GC roots)\n\
     stackmap:\n{stackmap}"
  );

  // 4) Validate that the record's instruction offset is the return address.
  //
  // LLVM's stackmap record uses the address of the instruction *after* the call.
  // We compute it by disassembling and taking the address of the instruction
  // immediately following the first `call*` in `@test`.
  let mut cmd = Command::new("llvm-objdump-18");
  cmd.arg("-d").arg("--no-show-raw-insn").arg(&obj);
  let disasm = run_success(cmd);

  let expected_inst_offset =
    parse_call_return_offset_from_objdump(&disasm, "test").expect("failed to parse disassembly");

  assert_eq!(
    record_inst_offset, expected_inst_offset,
    "stackmap instruction offset should be the call return address (next instruction)\n\
     record offset: {record_inst_offset}\n\
     disasm-derived: {expected_inst_offset}\n\
     disassembly:\n{disasm}\n\
     stackmap:\n{stackmap}"
  );
}
