//! Best-effort test for LLVM statepoint stackmap structure.
//!
//! This does *not* require observing a non-trivial (base != derived) pair.
//! As of LLVM 18.1, `rewrite-statepoints-for-gc` tends to rematerialize derived
//! pointers from the relocated base pointer, resulting in duplicate base/derived
//! stackmap locations even when the IR contains an interior pointer.
//!
//! We still parse the emitted `.llvm_stackmaps` section and assert the layout we
//! rely on:
//! - 3 header constant locations
//! - then 2 locations per live GC pointer (base + derived)

use std::fs;
use std::io;
use std::process::Command;

use tempfile::TempDir;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Location {
  loc_type: u8,
  size: u16,
  reg: u16,
  offset: i32,
}

struct Cursor<'a> {
  bytes: &'a [u8],
  pos: usize,
}

impl<'a> Cursor<'a> {
  fn new(bytes: &'a [u8]) -> Self {
    Self { bytes, pos: 0 }
  }

  fn remaining(&self) -> usize {
    self.bytes.len().saturating_sub(self.pos)
  }

  fn read_exact<const N: usize>(&mut self) -> io::Result<[u8; N]> {
    if self.remaining() < N {
      return Err(io::Error::new(
        io::ErrorKind::UnexpectedEof,
        "unexpected EOF while parsing stackmap section",
      ));
    }
    let mut out = [0u8; N];
    out.copy_from_slice(&self.bytes[self.pos..self.pos + N]);
    self.pos += N;
    Ok(out)
  }

  fn read_u8(&mut self) -> io::Result<u8> {
    Ok(self.read_exact::<1>()?[0])
  }

  fn read_u16_le(&mut self) -> io::Result<u16> {
    Ok(u16::from_le_bytes(self.read_exact::<2>()?))
  }

  fn read_u32_le(&mut self) -> io::Result<u32> {
    Ok(u32::from_le_bytes(self.read_exact::<4>()?))
  }

  fn read_i32_le(&mut self) -> io::Result<i32> {
    Ok(i32::from_le_bytes(self.read_exact::<4>()?))
  }

  fn read_u64_le(&mut self) -> io::Result<u64> {
    Ok(u64::from_le_bytes(self.read_exact::<8>()?))
  }

  fn align_to_8(&mut self) -> io::Result<()> {
    while self.pos % 8 != 0 {
      self.read_u8()?;
    }
    Ok(())
  }
}

fn try_run(cmd: &mut Command) -> io::Result<()> {
  let status = cmd.status()?;
  if !status.success() {
    return Err(io::Error::new(
      io::ErrorKind::Other,
      format!("command failed: {cmd:?} (status {status})"),
    ));
  }
  Ok(())
}

fn find_llvm_tool(name: &str) -> Option<String> {
  let candidates = [format!("{name}-18"), name.to_string()];
  for candidate in candidates {
    let output = Command::new(&candidate).arg("--version").output();
    let output = match output {
      Ok(output) if output.status.success() => output,
      _ => continue,
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let output = format!("{stdout}{stderr}").to_ascii_lowercase();
    if output.contains("version 18.") {
      return Some(candidate);
    }
  }
  None
}

fn dump_section(
  llvm_objcopy: &str,
  obj: &std::path::Path,
  out: &std::path::Path,
) -> io::Result<()> {
  // Linux/ELF.
  if try_run(
    Command::new(llvm_objcopy)
      .arg("--dump-section")
      .arg(format!(".llvm_stackmaps={}", out.display()))
      .arg(obj),
  )
  .is_ok()
  {
    return Ok(());
  }

  // macOS/Mach-O (for completeness).
  try_run(
    Command::new(llvm_objcopy)
      .arg("--dump-section")
      .arg(format!("__LLVM_STACKMAPS={}", out.display()))
      .arg(obj),
  )
}

fn parse_first_record_locations(stackmaps: &[u8]) -> io::Result<Vec<Location>> {
  let mut cur = Cursor::new(stackmaps);

  let version = cur.read_u8()?;
  if version != 3 {
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      format!("unexpected stackmap version {version} (expected 3)"),
    ));
  }

  // Reserved.
  cur.read_exact::<3>()?;

  let num_functions = cur.read_u32_le()? as usize;
  let num_constants = cur.read_u32_le()? as usize;
  let num_records = cur.read_u32_le()? as usize;

  if num_records == 0 {
    return Err(io::Error::new(
      io::ErrorKind::InvalidData,
      "expected at least one stackmap record",
    ));
  }

  // Function entries.
  for _ in 0..num_functions {
    cur.read_u64_le()?; // addr (relocated)
    cur.read_u64_le()?; // stack size
    cur.read_u64_le()?; // record count
  }

  // Constant pool.
  for _ in 0..num_constants {
    cur.read_u64_le()?;
  }

  // Only parse first record.
  cur.read_u64_le()?; // patchpoint id
  cur.read_u32_le()?; // instruction offset
  cur.read_u16_le()?; // reserved
  let num_locations = cur.read_u16_le()? as usize;

  let mut locs = Vec::with_capacity(num_locations);
  for _ in 0..num_locations {
    let loc_type = cur.read_u8()?;
    cur.read_u8()?; // reserved0
    let size = cur.read_u16_le()?;
    let reg = cur.read_u16_le()?;
    cur.read_u16_le()?; // reserved1
    let offset = cur.read_i32_le()?;
    locs.push(Location {
      loc_type,
      size,
      reg,
      offset,
    });
  }

  // The rest isn't needed for this test; but advance cursor to ensure we didn't
  // desync on a malformed location layout.
  let num_live_outs = cur.read_u16_le()? as usize;
  for _ in 0..num_live_outs {
    cur.read_u16_le()?; // reg
    cur.read_u8()?; // reserved
    cur.read_u8()?; // size
  }
  cur.align_to_8()?;

  Ok(locs)
}

#[test]
fn llvm_statepoint_stackmap_has_header_and_pairs() -> io::Result<()> {
  let opt = match find_llvm_tool("opt") {
    Some(opt) => opt,
    None => {
      eprintln!("skipping: LLVM 18 `opt` not available");
      return Ok(());
    }
  };
  let llc = match find_llvm_tool("llc") {
    Some(llc) => llc,
    None => {
      eprintln!("skipping: LLVM 18 `llc` not available");
      return Ok(());
    }
  };
  let llvm_objcopy = match find_llvm_tool("llvm-objcopy") {
    Some(objcopy) => objcopy,
    None => {
      eprintln!("skipping: LLVM 18 `llvm-objcopy` not available");
      return Ok(());
    }
  };

  let td = TempDir::new()?;
  let input_ll = td.path().join("input.ll");
  let rewritten_ll = td.path().join("rewritten.ll");
  let obj = td.path().join("out.o");
  let stackmaps_bin = td.path().join("stackmaps.bin");

  // Try to produce an interior pointer and keep it live at the safepoint.
  //
  // Even if LLVM chooses to rematerialize interior pointers (base==derived in
  // the stackmap), we still want to validate the (3 header, then pairs) layout.
  fs::write(
    &input_ll,
    r#"; ModuleID = 'statepoint'
source_filename = "statepoint"

declare void @callee(ptr addrspace(1))

define void @foo(ptr addrspace(1) %obj) gc "coreclr" {
entry:
  %derived = getelementptr i8, ptr addrspace(1) %obj, i64 16
  call void @callee(ptr addrspace(1) %derived) ["gc-live"(ptr addrspace(1) %derived)]
  ret void
}
"#,
  )?;

  try_run(
    Command::new(&opt)
      .arg("-passes=rewrite-statepoints-for-gc")
      .arg("-S")
      .arg(&input_ll)
      .arg("-o")
      .arg(&rewritten_ll),
  )?;

  try_run(
    Command::new(&llc)
      .arg("-filetype=obj")
      .arg(&rewritten_ll)
      .arg("-o")
      .arg(&obj),
  )?;

  dump_section(&llvm_objcopy, &obj, &stackmaps_bin)?;
  let stackmaps = fs::read(&stackmaps_bin)?;
  let locs = parse_first_record_locations(&stackmaps)?;

  assert!(
    locs.len() >= 3,
    "expected at least 3 header locations, got {}",
    locs.len()
  );

  let tail = &locs[3..];
  assert!(
    tail.len() >= 2,
    "expected at least one GC pointer pair after header, got {} locations",
    tail.len()
  );
  assert!(
    tail.len() % 2 == 0,
    "expected an even number of locations after header, got {}",
    tail.len()
  );

  // Count how many pairs appear to be truly base/derived (different location entries).
  let mut nontrivial_pairs = 0usize;
  for pair in tail.chunks_exact(2) {
    let base = pair[0];
    let derived = pair[1];
    if base != derived {
      nontrivial_pairs += 1;
    }
  }

  // We intentionally don't fail if LLVM doesn't emit any non-trivial pairs yet.
  // The pure-Rust unit tests cover the relocation math for future compiler changes.
  if nontrivial_pairs == 0 {
    eprintln!(
      "note: LLVM emitted only duplicate base/derived locations; interior pointers appear to be rematerialized"
    );
  }

  Ok(())
}
