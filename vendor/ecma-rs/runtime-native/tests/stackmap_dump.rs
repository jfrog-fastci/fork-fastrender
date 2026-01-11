use assert_cmd::Command;
use object::write::Object;
use object::{Architecture, BinaryFormat, Endianness, SectionKind};
use std::fs;
use std::path::Path;
use std::time::Duration;

fn stackmap_dump() -> Command {
  assert_cmd::cargo::cargo_bin_cmd!("stackmap-dump")
}

fn write_elf_with_section(path: &Path, section_name: &str, data: &[u8]) {
  let mut obj = Object::new(BinaryFormat::Elf, Architecture::X86_64, Endianness::Little);
  let sec = obj.add_section(Vec::new(), section_name.as_bytes().to_vec(), SectionKind::ReadOnlyData);
  obj.append_section_data(sec, data, 8);
  let bytes = obj.write().expect("write ELF object");
  fs::write(path, bytes).expect("write ELF file");
}

#[test]
fn summary_smoke() {
  let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("tests")
    .join("fixtures")
    .join("simple_stackmap.bin");

  let assert = stackmap_dump()
    .timeout(Duration::from_secs(5))
    .arg("--summary")
    .arg(&fixture)
    .assert()
    .success();

  let stdout = String::from_utf8_lossy(&assert.get_output().stdout);
  assert!(stdout.contains("StackMap v3"), "stdout was:\n{stdout}");
  assert!(stdout.contains("functions: 1"), "stdout was:\n{stdout}");
  assert!(stdout.contains("records: 1"), "stdout was:\n{stdout}");
  assert!(
    stdout.contains("addr=0x0000000000001000"),
    "stdout was:\n{stdout}"
  );
}

#[test]
fn records_json_smoke() {
  let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("tests")
    .join("fixtures")
    .join("simple_stackmap.bin");

  let assert = stackmap_dump()
    .timeout(Duration::from_secs(5))
    .arg("--records")
    .arg("--json")
    .arg(&fixture)
    .assert()
    .success();

  let v: serde_json::Value =
    serde_json::from_slice(&assert.get_output().stdout).expect("stdout should be valid JSON");
  assert_eq!(v["mode"], "records");
  assert_eq!(v["version"], 3);
  assert_eq!(v["records"].as_array().unwrap().len(), 1);
  assert_eq!(v["records"][0]["callsite_address"], "0x0000000000001010");
  assert_eq!(v["records"][0]["locations"].as_array().unwrap().len(), 2);
}

#[test]
fn reads_stackmaps_from_elf_data_rel_ro_section_and_parses_all_blobs() {
  let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("tests")
    .join("fixtures")
    .join("simple_stackmap.bin");
  let blob = fs::read(&fixture).expect("read simple_stackmap.bin");

  // Simulate linker concatenation: two complete StackMap v3 blobs back-to-back with padding.
  let mut section = Vec::new();
  section.extend_from_slice(&blob);
  section.extend_from_slice(&[0u8; 8]); // linker alignment padding
  section.extend_from_slice(&blob);

  let td = tempfile::tempdir().expect("create tempdir");
  let elf_path = td.path().join("stackmaps.o");
  write_elf_with_section(&elf_path, ".data.rel.ro.llvm_stackmaps", &section);

  let assert = stackmap_dump()
    .timeout(Duration::from_secs(5))
    .arg("--summary")
    .arg("--json")
    .arg(&elf_path)
    .assert()
    .success();

  let v: serde_json::Value =
    serde_json::from_slice(&assert.get_output().stdout).expect("stdout should be valid JSON");
  assert_eq!(v["mode"], "summary");
  assert_eq!(v["version"], 3);
  assert_eq!(v["num_stackmaps"], 2);
  assert_eq!(v["functions"].as_array().unwrap().len(), 2);
}

#[test]
fn reads_stackmaps_from_elf_section_without_leading_dot() {
  let fixture = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    .join("tests")
    .join("fixtures")
    .join("simple_stackmap.bin");
  let blob = fs::read(&fixture).expect("read simple_stackmap.bin");

  let td = tempfile::tempdir().expect("create tempdir");
  let elf_path = td.path().join("stackmaps.o");
  write_elf_with_section(&elf_path, "llvm_stackmaps", &blob);

  let assert = stackmap_dump()
    .timeout(Duration::from_secs(5))
    .arg("--summary")
    .arg("--json")
    .arg(&elf_path)
    .assert()
    .success();

  let v: serde_json::Value =
    serde_json::from_slice(&assert.get_output().stdout).expect("stdout should be valid JSON");
  assert_eq!(v["mode"], "summary");
  assert_eq!(v["version"], 3);
  assert_eq!(v["num_stackmaps"], 1);
}
