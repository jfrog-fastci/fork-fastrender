#![cfg(target_os = "linux")]

use anyhow::{bail, Context, Result};
use runtime_native::stackmaps::{Location, StackMaps};
use std::path::Path;
use std::process::Command;

fn have_tool(tool: &str) -> bool {
  Command::new(tool).arg("--version").output().is_ok()
}

fn run(cmd: &mut Command) -> Result<()> {
  let output = cmd.output().with_context(|| format!("spawn {cmd:?}"))?;
  if !output.status.success() {
    bail!(
      "command failed: {cmd:?}\nstatus: {}\nstdout:\n{}\nstderr:\n{}",
      output.status,
      String::from_utf8_lossy(&output.stdout),
      String::from_utf8_lossy(&output.stderr)
    );
  }
  Ok(())
}

fn read_elf64_section(path: &Path, wanted: &str) -> Result<Vec<u8>> {
  let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
  if &bytes[0..4] != b"\x7fELF" {
    bail!("not an ELF file: {}", path.display());
  }
  if bytes[4] != 2 {
    bail!("not an ELF64 file: {}", path.display());
  }
  if bytes[5] != 1 {
    bail!("not a little-endian ELF file: {}", path.display());
  }

  let e_shoff = u64::from_le_bytes(bytes[0x28..0x30].try_into().unwrap()) as usize;
  let e_shentsize = u16::from_le_bytes(bytes[0x3A..0x3C].try_into().unwrap()) as usize;
  let e_shnum = u16::from_le_bytes(bytes[0x3C..0x3E].try_into().unwrap()) as usize;
  let e_shstrndx = u16::from_le_bytes(bytes[0x3E..0x40].try_into().unwrap()) as usize;

  let shstr = read_shdr(&bytes, e_shoff, e_shentsize, e_shstrndx)?;
  let strtab_start = shstr.sh_offset as usize;
  let strtab_end = strtab_start + (shstr.sh_size as usize);
  let strtab = &bytes[strtab_start..strtab_end];

  for i in 0..e_shnum {
    let sh = read_shdr(&bytes, e_shoff, e_shentsize, i)?;
    let name = get_str(strtab, sh.sh_name)?;
    if name == wanted {
      let start = sh.sh_offset as usize;
      let end = start + (sh.sh_size as usize);
      return Ok(bytes[start..end].to_vec());
    }
  }
  bail!("section {wanted} not found in {}", path.display());
}

#[derive(Clone, Copy)]
struct Shdr {
  sh_name: u32,
  sh_offset: u64,
  sh_size: u64,
}

fn read_shdr(bytes: &[u8], e_shoff: usize, e_shentsize: usize, idx: usize) -> Result<Shdr> {
  let start = e_shoff + idx * e_shentsize;
  let hdr = &bytes[start..start + 0x40];
  Ok(Shdr {
    sh_name: u32::from_le_bytes(hdr[0..4].try_into().unwrap()),
    sh_offset: u64::from_le_bytes(hdr[0x18..0x20].try_into().unwrap()),
    sh_size: u64::from_le_bytes(hdr[0x20..0x28].try_into().unwrap()),
  })
}

fn get_str<'a>(strtab: &'a [u8], off: u32) -> Result<&'a str> {
  let start = off as usize;
  let rest = &strtab[start..];
  let nul = rest.iter().position(|&b| b == 0).context("unterminated string")?;
  std::str::from_utf8(&rest[..nul]).context("section name not utf8")
}

fn parse_objdump_instructions(disasm: &str) -> Vec<(u64, String)> {
  let mut out = Vec::new();
  for line in disasm.lines() {
    let line = line.trim_start();
    let Some((addr_str, rest)) = line.split_once(':') else {
      continue;
    };
    if addr_str.is_empty() || !addr_str.chars().all(|c| c.is_ascii_hexdigit()) {
      continue;
    }
    let Ok(addr) = u64::from_str_radix(addr_str, 16) else {
      continue;
    };
    out.push((addr, rest.trim().to_string()));
  }
  out
}

#[test]
fn parse_statepoint_stackmap_v3_and_index_by_return_address() -> Result<()> {
  if !have_tool("opt-18") || !have_tool("llc-18") || !have_tool("llvm-objdump-18") {
    eprintln!("skipping: LLVM 18 tools (opt-18/llc-18/llvm-objdump-18) not available");
    return Ok(());
  }

  let td = tempfile::tempdir().context("tempdir")?;
  let input_ll = td.path().join("input.ll");
  let rewritten_ll = td.path().join("rewritten.ll");
  let obj = td.path().join("out.o");

  std::fs::write(
    &input_ll,
    r#"
      target triple = "x86_64-pc-linux-gnu"

      declare void @callee(i64)

      define void @foo(ptr addrspace(1) %p) gc "coreclr" {
      entry:
        call void @callee(i64 1) ["gc-live"(ptr addrspace(1) %p)]
        ret void
      }
    "#,
  )
  .context("write input.ll")?;

  run(
    Command::new("opt-18")
      .arg("-passes=rewrite-statepoints-for-gc")
      .arg("-S")
      .arg(&input_ll)
      .arg("-o")
      .arg(&rewritten_ll),
  )?;

  run(
    Command::new("llc-18")
      .arg("-O0")
      .arg("--frame-pointer=all")
      // runtime-native requires statepoint roots to be spilled to stack slots.
      .arg("--fixup-allow-gcptr-in-csr=false")
      .arg("--fixup-max-csr-statepoints=0")
      .arg("-filetype=obj")
      .arg(&rewritten_ll)
      .arg("-o")
      .arg(&obj),
  )?;

  let stackmaps_bytes = read_elf64_section(&obj, ".llvm_stackmaps")?;
  let stackmaps = StackMaps::parse(&stackmaps_bytes).context("parse .llvm_stackmaps bytes")?;

  assert_eq!(stackmaps.raw().version, 3);
  assert_eq!(stackmaps.callsites().len(), 1);

  let (pc, callsite) = stackmaps.iter().next().unwrap();
  let record = callsite.record;

  // Verify statepoint location layout: 3 constants + 1 base/derived pair.
  assert_eq!(record.locations.len(), 5);
  assert!(callsite.stack_size > 0);
  for i in 0..3 {
    assert_eq!(
      record.locations[i],
      Location::Constant { size: 8, value: 0 }
    );
  }
  assert!(matches!(record.locations[3], Location::Indirect { .. }));
  assert!(matches!(record.locations[4], Location::Indirect { .. }));

  // Verify that the stackmap's instruction_offset points at the return address after the callq.
  // We do this by matching it against the next instruction address following a call.
  let disasm = Command::new("llvm-objdump-18")
    .arg("-d")
    .arg("--no-show-raw-insn")
    .arg(&obj)
    .output()
    .context("run llvm-objdump-18")?;
  if !disasm.status.success() {
    bail!(
      "llvm-objdump-18 failed:\nstdout:\n{}\nstderr:\n{}",
      String::from_utf8_lossy(&disasm.stdout),
      String::from_utf8_lossy(&disasm.stderr)
    );
  }
  let disasm = String::from_utf8_lossy(&disasm.stdout);
  let insns = parse_objdump_instructions(&disasm);

  let mut matched = false;
  for win in insns.windows(2) {
    let (addr, text) = (&win[0].0, &win[0].1);
    let next_addr = win[1].0;
    if next_addr == record.instruction_offset as u64 && text.contains("call") {
      matched = true;
      // In a relocatable object, function addresses are usually 0 + relocation, so PC==offset.
      assert_eq!(pc, record.instruction_offset as u64);
      eprintln!("matched call at {addr:#x}: {text}");
      break;
    }
  }
  assert!(matched, "did not find call instruction ending at stackmap return address");

  Ok(())
}

#[test]
fn parse_constindex_locations_and_constants_pool() -> Result<()> {
  if !have_tool("llc-18") {
    eprintln!("skipping: llc-18 not available");
    return Ok(());
  }

  let td = tempfile::tempdir().context("tempdir")?;
  let input_ll = td.path().join("input.ll");
  let obj = td.path().join("out.o");

  std::fs::write(
    &input_ll,
    r#"
      declare void @llvm.experimental.stackmap(i64, i32, ...)

      define void @foo() {
      entry:
        call void (i64, i32, ...) @llvm.experimental.stackmap(i64 1, i32 0, i64 1311768467463790320) ; 0x123456789abcdef0
        ret void
      }
    "#,
  )
  .context("write input.ll")?;

  run(
    Command::new("llc-18")
      .arg("-O0")
      .arg("--frame-pointer=all")
      .arg("-filetype=obj")
      .arg(&input_ll)
      .arg("-o")
      .arg(&obj),
  )?;

  let stackmaps_bytes = read_elf64_section(&obj, ".llvm_stackmaps")?;
  let stackmaps = StackMaps::parse(&stackmaps_bytes).context("parse stackmaps")?;

  assert_eq!(stackmaps.raw().version, 3);
  assert_eq!(stackmaps.raw().constants.len(), 1);
  assert_eq!(stackmaps.callsites().len(), 1);

  let (_pc, callsite) = stackmaps.iter().next().unwrap();
  let record = callsite.record;
  assert_eq!(record.locations.len(), 1);
  let loc = &record.locations[0];
  assert_eq!(
    *loc,
    Location::ConstIndex {
      size: 8,
      index: 0,
      value: 0x1234_5678_9abc_def0
    }
  );

  Ok(())
}
