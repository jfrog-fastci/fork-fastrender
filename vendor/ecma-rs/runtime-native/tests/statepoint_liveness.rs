#![cfg(all(target_arch = "x86_64", target_os = "linux"))]

use object::{Object, ObjectSection};
use runtime_native::stackmaps::{StackMap, StackMaps};
use runtime_native::statepoints::LLVM18_STATEPOINT_HEADER_CONSTANTS;
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

fn tool_available(tool: &str) -> bool {
  Command::new(tool)
    .arg("--version")
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .status()
    .is_ok()
}

fn run_success(mut cmd: Command) -> Vec<u8> {
  let cmd_str = format!("{cmd:?}");
  let out = cmd.output().unwrap_or_else(|e| panic!("failed to run {cmd_str}: {e}"));
  if !out.status.success() {
    panic!(
      "command failed: {cmd_str}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
      out.status,
      String::from_utf8_lossy(&out.stdout),
      String::from_utf8_lossy(&out.stderr),
    );
  }
  out.stdout
}

fn parse_gc_live_bundles(ir: &str) -> Vec<Vec<String>> {
  const MARKER: &str = "\"gc-live\"(";

  let mut out: Vec<Vec<String>> = Vec::new();

  let bytes = ir.as_bytes();
  let mut search_from: usize = 0;
  while let Some(rel) = ir[search_from..].find(MARKER) {
    let start = search_from + rel;
    let mut idx = start + MARKER.len();

    // `MARKER` includes the opening `(`.
    let mut depth: i32 = 1;
    while idx < bytes.len() {
      match bytes[idx] {
        b'(' => depth += 1,
        b')' => {
          depth -= 1;
          if depth == 0 {
            break;
          }
        }
        _ => {}
      }
      idx += 1;
    }
    assert_eq!(
      depth, 0,
      "unterminated gc-live operand bundle starting at byte {start}:\n{ir}"
    );

    let inner = &ir[(start + MARKER.len())..idx];
    let vars: Vec<String> = inner
      .split(',')
      .map(str::trim)
      .filter(|s| !s.is_empty())
      .map(|s| {
        s.split_whitespace()
          .last()
          .unwrap_or_else(|| panic!("malformed gc-live entry `{s}` in:\n{ir}"))
          .to_string()
      })
      .collect();
    out.push(vars);

    search_from = idx + 1;
  }

  out
}

fn stackmaps_section_from_obj(obj_path: &Path) -> Vec<u8> {
  let bytes = fs::read(obj_path).expect("read llc output object");
  let obj = object::File::parse(&*bytes).expect("parse llc output object");
  let section = obj
    .section_by_name(".llvm_stackmaps")
    .expect("missing .llvm_stackmaps section in llc output");
  section
    .data()
    .expect("read .llvm_stackmaps section bytes")
    .to_vec()
}

#[test]
fn rewrite_statepoints_for_gc_expands_gc_live_by_liveness() {
  for tool in ["opt-18", "llc-18"] {
    if !tool_available(tool) {
      eprintln!("skipping: {tool} not available in PATH");
      return;
    }
  }

  // IR intentionally omits `%b` from the first call's `"gc-live"` bundle even
  // though `%b` is live across the call (it is used at the second call and in
  // the return). LLVM's `rewrite-statepoints-for-gc` pass expands `"gc-live"` to
  // the full set of live `ptr addrspace(1)` GC references at each safepoint.
  //
  // Both calls are assigned the *same* `"statepoint-id"` so the resulting
  // StackMap v3 records intentionally share `patchpoint_id` but have different
  // `instruction_offset`s.
  const INPUT_IR: &str = r#"
target triple = "x86_64-unknown-linux-gnu"

declare void @callee()

define ptr addrspace(1) @test(ptr addrspace(1) %a, ptr addrspace(1) %b) gc "coreclr" {
entry:
  call void @callee() #0 [ "gc-live"(ptr addrspace(1) %a) ]
  %isnull = icmp eq ptr addrspace(1) %a, null
  call void @callee() #0 [ "gc-live"(ptr addrspace(1) %b) ]
  %out = select i1 %isnull, ptr addrspace(1) %b, ptr addrspace(1) null
  ret ptr addrspace(1) %out
}

attributes #0 = { "statepoint-id"="2882400000" }
"#;

  let tmp = tempfile::tempdir().expect("create tempdir");
  let input_ll = tmp.path().join("liveness.ll");
  let rewritten_ll = tmp.path().join("liveness.rewritten.ll");
  let obj_path = tmp.path().join("liveness.o");

  fs::write(&input_ll, INPUT_IR).expect("write input IR");

  let mut opt = Command::new("opt-18");
  opt
    .arg("-S")
    .arg("-passes=rewrite-statepoints-for-gc")
    .arg(&input_ll);
  let rewritten_ir = String::from_utf8(run_success(opt)).expect("opt output should be UTF-8");

  let bundles = parse_gc_live_bundles(&rewritten_ir);
  assert!(
    bundles.len() >= 2,
    "expected >= 2 gc-live operand bundles after rewriting, got {}.\n\nRewritten IR:\n{rewritten_ir}",
    bundles.len()
  );

  let first_set: HashSet<&str> = bundles[0].iter().map(|s| s.as_str()).collect();
  assert!(
    first_set.contains("%a") && first_set.contains("%b"),
    "expected first rewritten statepoint gc-live to contain both %a and %b (order doesn't matter)\n\
     got: {:?}\n\nRewritten IR:\n{rewritten_ir}",
    bundles[0]
  );

  // Keep the rewritten IR around for llc.
  fs::write(&rewritten_ll, &rewritten_ir).expect("write rewritten IR");

  let mut llc = Command::new("llc-18");
  llc
    .arg("-O0")
    .arg("--frame-pointer=all")
    .arg("-filetype=obj")
    .arg(&rewritten_ll)
    .arg("-o")
    .arg(&obj_path);
  run_success(llc);

  let stackmap_bytes = stackmaps_section_from_obj(&obj_path);

  // Parse the raw StackMap section and assert that the record's location count
  // matches liveness (not the original `"gc-live"` bundle text).
  let sm = StackMap::parse(&stackmap_bytes).expect("parse .llvm_stackmaps");
  assert_eq!(
    sm.records.len(),
    2,
    "expected exactly 2 stackmap records (one per callsite), got {}",
    sm.records.len()
  );

  let mut records: Vec<_> = sm.records.iter().collect();
  records.sort_by_key(|r| r.instruction_offset);
  let first = records[0];
  let second = records[1];

  assert_eq!(
    first.locations.len(),
    LLVM18_STATEPOINT_HEADER_CONSTANTS + 2 * 2,
    "first statepoint should record 2 live GC pointers (base/derived pairs) after liveness expansion"
  );
  assert_eq!(
    second.locations.len(),
    LLVM18_STATEPOINT_HEADER_CONSTANTS + 2 * 1,
    "second statepoint should record 1 live GC pointer"
  );

  assert_eq!(
    first.patchpoint_id, second.patchpoint_id,
    "patchpoint_id is not unique across callsites; expected duplicates"
  );
  assert_ne!(
    first.instruction_offset, second.instruction_offset,
    "different callsites must have different instruction offsets"
  );

  // Runtime lookup must be keyed by the callsite return address (PC), not by
  // `patchpoint_id`.
  let indexed = StackMaps::parse(&stackmap_bytes).expect("build StackMaps PC index");
  assert_eq!(
    indexed.callsites().len(),
    2,
    "PC-indexed StackMaps must preserve both callsites even if patchpoint_id repeats"
  );

  let entries = indexed.callsites();
  assert_ne!(
    entries[0].record_index, entries[1].record_index,
    "PC index should contain two distinct record indices"
  );
  assert_eq!(
    indexed.raw().records[entries[0].record_index].patchpoint_id,
    indexed.raw().records[entries[1].record_index].patchpoint_id,
    "both indexed callsites should share the same patchpoint_id"
  );
  assert_ne!(
    entries[0].pc, entries[1].pc,
    "callsite PCs must differ even if patchpoint_id repeats"
  );

  let cs0 = indexed.lookup(entries[0].pc).expect("lookup first callsite");
  let cs1 = indexed.lookup(entries[1].pc).expect("lookup second callsite");
  assert_ne!(
    cs0.record.instruction_offset, cs1.record.instruction_offset,
    "PC-based lookup should distinguish callsites with identical patchpoint_id"
  );
}
