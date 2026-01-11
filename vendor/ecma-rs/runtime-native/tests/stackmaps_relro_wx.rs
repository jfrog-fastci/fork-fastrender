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
  for tool in [
    "llc-18",
    "llvm-objcopy-18",
    "llvm-readobj-18",
    "gcc",
    "readelf",
    "bash",
  ] {
    if !cmd_exists(tool) {
      eprintln!("skipping: missing tool {tool}");
      return;
    }
  }

  // This test relies on GNU ld semantics for RELRO and section placement. If
  // the toolchain is configured to use lld by default, skip rather than failing
  // on a linker-script compatibility mismatch.
  let linker_version_out = Command::new("gcc")
    .args(["-Wl,--version"])
    .output()
    .unwrap_or_else(|e| panic!("failed to query linker version via gcc: {e}"));
  let linker_version = format!(
    "{}{}",
    String::from_utf8_lossy(&linker_version_out.stdout),
    String::from_utf8_lossy(&linker_version_out.stderr),
  );
  if linker_version.to_ascii_lowercase().contains("lld") {
    eprintln!("skipping: gcc appears to be using lld; GNU ld is required for this RELRO placement test");
    return;
  }

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
    "llc-18",
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
  let sections_before = output("llvm-readobj-18", &["--sections", foo_o.to_str().unwrap()]);
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
  // This test links with GNU ld via `gcc`. For PIE builds, GNU ld may merge
  // writable `.data.rel.ro.*` output sections into the same PT_LOAD as `.text`
  // if they are inserted "after .text", producing an RWX segment. Use the
  // GNU ld-specific fragment that inserts into the RELRO/data region instead.
  let linker_script = manifest_dir.join("link/stackmaps_gnuld.ld");
  assert!(
    linker_script.exists(),
    "missing linker script at {linker_script:?}"
  );

  run("bash", &[rename_script.to_str().unwrap(), foo_o.to_str().unwrap()]);

  // Sanity check: the rename succeeded so the test truly exercises `.data.rel.ro.*` placement.
  let sections_after = output("llvm-readobj-18", &["--sections", foo_o.to_str().unwrap()]);
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

  // Parse program headers in order so we can map a PT_GNU_RELRO entry to the
  // corresponding "Section to Segment mapping" index.
  let mut phdr_types: Vec<String> = Vec::new();
  let mut load_lines: Vec<String> = Vec::new();
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
    phdr_types.push(typ.to_string());
    if typ == "LOAD" {
      load_lines.push(trimmed.to_string());
    }
  }

  assert!(
    !phdr_types.is_empty(),
    "failed to parse readelf -l program headers; got:\n{segments}"
  );

  let relro_index = phdr_types
    .iter()
    .position(|t| t == "GNU_RELRO")
    .expect("expected a PT_GNU_RELRO program header");

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

  let mut in_mapping = false;
  let mut current_seg: Option<usize> = None;
  let mut seg_to_sections: std::collections::HashMap<usize, String> = std::collections::HashMap::new();
  for line in segments.lines() {
    if line.trim() == "Section to Segment mapping:" {
      in_mapping = true;
      continue;
    }
    if !in_mapping {
      continue;
    }

    let trimmed = line.trim_start();
    if trimmed.is_empty() {
      continue;
    }
    if trimmed.starts_with("Segment") {
      continue;
    }

    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    if tokens.is_empty() {
      continue;
    }

    if tokens[0].chars().all(|c| c.is_ascii_digit()) {
      let idx = tokens[0]
        .parse::<usize>()
        .unwrap_or_else(|e| panic!("invalid segment index in readelf mapping line {trimmed:?}: {e}"));
      current_seg = Some(idx);
      seg_to_sections.insert(idx, tokens[1..].join(" "));
    } else if let Some(idx) = current_seg {
      seg_to_sections
        .entry(idx)
        .and_modify(|s| {
          if !s.is_empty() {
            s.push(' ');
          }
          s.push_str(trimmed);
        })
        .or_insert_with(|| trimmed.to_string());
    }
  }

  let relro_sections = seg_to_sections.get(&relro_index).unwrap_or_else(|| {
    panic!(
      "failed to find segment index {relro_index} (PT_GNU_RELRO) in section mapping; got:\n{segments}"
    )
  });
  assert!(
    relro_sections.contains(".data.rel.ro.llvm_stackmaps"),
    "expected `.data.rel.ro.llvm_stackmaps` to be covered by PT_GNU_RELRO; segment {relro_index} sections were:\n{relro_sections}\n\nfull readelf -l output:\n{segments}"
  );
}
