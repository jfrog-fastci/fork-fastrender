#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn cmd_exists(cmd: &str) -> bool {
  Command::new(cmd)
    .arg("--version")
    .stdout(std::process::Stdio::null())
    .stderr(std::process::Stdio::null())
    .status()
    .map(|s| s.success())
    .unwrap_or(false)
}

fn find_tool(candidates: &[&'static str]) -> Option<&'static str> {
  for &cand in candidates {
    if cmd_exists(cand) {
      return Some(cand);
    }
  }
  None
}

fn run(cmd: &str, args: &[&str]) {
  let status = Command::new(cmd)
    .args(args)
    .status()
    .unwrap_or_else(|e| panic!("failed to run {cmd}: {e}"));
  assert!(
    status.success(),
    "{cmd} {:?} failed with status {status}",
    args
  );
}

fn output(cmd: &str, args: &[&str]) -> String {
  let out = Command::new(cmd)
    .args(args)
    .output()
    .unwrap_or_else(|e| panic!("failed to run {cmd}: {e}"));
  assert!(
    out.status.success(),
    "{cmd} {:?} failed with status {}",
    args,
    out.status
  );
  String::from_utf8_lossy(&out.stdout).into_owned()
}

fn write(path: &Path, contents: &str) {
  fs::write(path, contents).unwrap_or_else(|e| panic!("failed to write {path:?}: {e}"));
}

#[test]
fn pie_stackmaps_are_in_gnu_relro_and_no_load_segment_is_rwx() {
  for tool in ["ld.bfd", "gcc", "readelf", "bash"] {
    if !cmd_exists(tool) {
      eprintln!("skipping: missing tool {tool}");
      return;
    }
  }

  let Some(llc) = find_tool(&["llc-18", "llc"]) else {
    eprintln!("skipping: llc not found in PATH (need llc-18 or llc)");
    return;
  };
  // `rename_llvm_stackmaps_section.sh` uses llvm-readobj/llvm-objcopy, so ensure they're available
  // before spawning the helper.
  let Some(readobj) = find_tool(&["llvm-readobj-18", "llvm-readobj"]) else {
    eprintln!("skipping: llvm-readobj not found in PATH (need llvm-readobj-18 or llvm-readobj)");
    return;
  };
  let Some(_objcopy) = find_tool(&["llvm-objcopy-18", "llvm-objcopy"]) else {
    eprintln!("skipping: llvm-objcopy not found in PATH (need llvm-objcopy-18 or llvm-objcopy)");
    return;
  };

  // Use GNU ld (bfd) for this test: it specifically exercises the GNU-ld fragment
  // (`link/stackmaps_gnuld.ld`) which places stackmaps/faultmaps into dedicated
  // `.data.rel.ro.llvm_*` output sections and keeps them covered by `PT_GNU_RELRO`.
  //
  // lld uses a different fragment (`link/stackmaps.ld`) that appends the rewritten
  // `.data.rel.ro.llvm_*` *input* sections into the standard `.data.rel.ro` output section.

  let tmp = tempfile::tempdir().expect("tempdir");
  let dir = tmp.path();

  let foo_ll = dir.join("foo.ll");
  let foo_o = dir.join("foo.o");
  let main_c = dir.join("main.c");
  let main_o = dir.join("main.o");
  let exe = dir.join("a_pie");

  write(
    &foo_ll,
    r#"
target triple = "x86_64-unknown-linux-gnu"

declare void @llvm.experimental.stackmap(i64, i32, ...)

define void @foo() {
entry:
  call void (i64, i32, ...) @llvm.experimental.stackmap(i64 1, i32 0)
  ret void
}
"#,
  );

  run(
    llc,
    &[
      "-O0",
      "-filetype=obj",
      "-relocation-model=pic",
      foo_ll.to_str().unwrap(),
      "-o",
      foo_o.to_str().unwrap(),
    ],
  );

  // Sanity check: object contains the legacy section before rename.
  let sections_before = output(readobj, &["--sections", foo_o.to_str().unwrap()]);
  assert!(
    sections_before.contains(".llvm_stackmaps"),
    "expected foo.o to contain .llvm_stackmaps before rename; got:\n{sections_before}"
  );

  let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let repo_root = manifest_dir.parent().expect("runtime-native/..");
  let rename_script = repo_root.join("scripts/rename_llvm_stackmaps_section.sh");
  assert!(
    rename_script.exists(),
    "missing helper script at {rename_script:?}"
  );
  // This test links with `gcc` (GNU ld). Use the GNU ld-specific fragment to
  // avoid RWX LOAD segments when stackmaps must be writable during relocation.
  let linker_script = manifest_dir.join("link/stackmaps_gnuld.ld");
  assert!(
    linker_script.exists(),
    "missing linker script at {linker_script:?}"
  );

  run("bash", &[rename_script.to_str().unwrap(), foo_o.to_str().unwrap()]);

  // Sanity check: the rename succeeded so the test truly exercises `.data.rel.ro.*` placement.
  let sections_after = output(readobj, &["--sections", foo_o.to_str().unwrap()]);
  assert!(
    sections_after.contains(".data.rel.ro.llvm_stackmaps"),
    "expected foo.o to contain .data.rel.ro.llvm_stackmaps after rename; got:\n{sections_after}"
  );

  write(
    &main_c,
    r#"
void foo(void);
int main() {
  foo();
  return 0;
}
"#,
  );

  run(
    "gcc",
    &[
      "-fPIE",
      "-c",
      main_c.to_str().unwrap(),
      "-o",
      main_o.to_str().unwrap(),
    ],
  );

  let ld_arg = format!("-Wl,-T,{}", linker_script.display());
  run(
    "gcc",
    &[
      "-fuse-ld=bfd",
      "-pie",
      // Ensure PT_GNU_RELRO exists so we can assert stackmap coverage explicitly.
      "-Wl,-z,relro",
      // Regression guard: section GC drops unreferenced stackmaps unless the
      // linker script explicitly `KEEP()`s the section.
      "-Wl,--gc-sections",
      ld_arg.as_str(),
      main_o.to_str().unwrap(),
      foo_o.to_str().unwrap(),
      "-o",
      exe.to_str().unwrap(),
    ],
  );

  let dynamic = output("readelf", &["-d", exe.to_str().unwrap()]);
  assert!(
    !dynamic.contains("TEXTREL"),
    "expected no DT_TEXTREL; got:\n{dynamic}"
  );

  let segments = output("readelf", &["-W", "-l", exe.to_str().unwrap()]);

  let mut load_lines: Vec<String> = Vec::new();
  let mut relro_line: Option<String> = None;
  let mut in_phdrs = false;
  for line in segments.lines() {
    if line.trim() == "Program Headers:" {
      in_phdrs = true;
      continue;
    }
    if !in_phdrs {
      continue;
    }
    if line.trim() == "Section to Segment mapping:" {
      break;
    }
    let trimmed = line.trim_start();
    if trimmed.is_empty() {
      continue;
    }
    // Header row: "Type Offset VirtAddr ..."
    if trimmed.starts_with("Type") {
      continue;
    }
    // e.g. "  [Requesting program interpreter: ...]"
    if trimmed.starts_with('[') {
      continue;
    }

    let Some(typ) = trimmed.split_whitespace().next() else {
      continue;
    };
    if typ == "LOAD" {
      load_lines.push(trimmed.to_string());
    }
    if typ == "GNU_RELRO" {
      relro_line = Some(trimmed.to_string());
    }
  }

  let relro_line = relro_line
    .unwrap_or_else(|| panic!("expected a PT_GNU_RELRO program header; got:\n{segments}"));
  let relro_parts: Vec<&str> = relro_line.split_whitespace().collect();
  // Columns: Type, Offset, VirtAddr, PhysAddr, FileSiz, MemSiz, Flg..., Align
  assert!(
    relro_parts.len() >= 6,
    "failed to parse GNU_RELRO program header line: {relro_line}"
  );
  let relro_vaddr = u64::from_str_radix(relro_parts[2].trim_start_matches("0x"), 16)
    .unwrap_or_else(|e| panic!("invalid GNU_RELRO vaddr in {relro_line:?}: {e}"));
  let relro_memsz = u64::from_str_radix(relro_parts[5].trim_start_matches("0x"), 16)
    .unwrap_or_else(|e| panic!("invalid GNU_RELRO memsz in {relro_line:?}: {e}"));
  let relro_end = relro_vaddr
    .checked_add(relro_memsz)
    .expect("GNU_RELRO vaddr+memsz overflow");

  for load in &load_lines {
    let parts: Vec<&str> = load.split_whitespace().collect();
    // Columns: Type, Offset, VirtAddr, PhysAddr, FileSiz, MemSiz, Flg..., Align
    if parts.len() < 8 {
      continue;
    }
    let flags = &parts[6..parts.len() - 1];
    let has_w = flags.iter().any(|f| f.contains('W'));
    let has_e = flags.iter().any(|f| f.contains('E'));
    assert!(
      !(has_w && has_e),
      "found RWX LOAD segment (W+E) in linked PIE; offending header:\n{load}\n\nfull readelf -l output:\n{segments}"
    );
  }

  let syms = output("readelf", &["-W", "-s", exe.to_str().unwrap()]);
  let find_sym = |name: &str| -> u64 {
    for line in syms.lines() {
      let trimmed = line.trim();
      if trimmed.is_empty() {
        continue;
      }
      let parts: Vec<&str> = trimmed.split_whitespace().collect();
      if parts.len() < 2 || parts.last().copied() != Some(name) {
        continue;
      }
      return u64::from_str_radix(parts[1].trim_start_matches("0x"), 16)
        .unwrap_or_else(|e| panic!("invalid symbol value for {name} in {trimmed:?}: {e}"));
    }
    panic!("missing {name} symbol; readelf -s output:\n{syms}");
  };

  let start = find_sym("__start_llvm_stackmaps");
  let stop = find_sym("__stop_llvm_stackmaps");
  assert!(
    stop > start,
    "invalid stackmaps symbol range (start=0x{start:x} stop=0x{stop:x})"
  );
  assert!(
    relro_vaddr <= start && stop <= relro_end,
    "expected stackmaps range to be within PT_GNU_RELRO (relro=[0x{relro_vaddr:x},0x{relro_end:x}) start=0x{start:x} stop=0x{stop:x});\n\nfull readelf -l output:\n{segments}"
  );
}
