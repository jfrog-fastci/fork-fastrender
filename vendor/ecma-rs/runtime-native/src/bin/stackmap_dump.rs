use anyhow::{bail, Context, Result};
use clap::Parser;
use runtime_native::stackmaps::{parse_all_stackmaps, Location, StackMap};
use serde::Serialize;
use std::io::Write;
use std::path::PathBuf;

// Keep the stackmap-dump binary self-contained: a small ELF section reader lives
// under `src/bin/stackmap_dump/` so we don't expose it as part of
// `runtime-native`'s public library API.
#[path = "stackmap_dump/endian.rs"]
mod endian;
#[path = "stackmap_dump/elf.rs"]
mod elf;

#[derive(Parser)]
#[command(
  name = "stackmap-dump",
  about = "Inspect LLVM .llvm_stackmaps (statepoints)"
)]
struct Cli {
  /// Path to a binary/object containing an LLVM stackmaps section.
  ///
  /// If the file is not an ELF file, it is treated as raw `.llvm_stackmaps` section bytes.
  path: PathBuf,

  /// Print version, counts, and per-function record count (default).
  #[arg(long, conflicts_with = "records")]
  summary: bool,

  /// Print each callsite record and its locations.
  #[arg(long, conflicts_with = "summary")]
  records: bool,

  /// Only show records matching this callsite address (hex).
  #[arg(long, value_name = "HEX")]
  filter_addr: Option<String>,

  /// Emit machine-readable JSON to stdout.
  #[arg(long)]
  json: bool,

  /// Treat the input file as raw `.llvm_stackmaps` section bytes, even if it is an ELF file.
  #[arg(long)]
  raw_section: bool,
}

fn parse_hex_u64(s: &str) -> Result<u64> {
  let s = s.trim();
  let s = s
    .strip_prefix("0x")
    .or_else(|| s.strip_prefix("0X"))
    .unwrap_or(s);
  u64::from_str_radix(s, 16).with_context(|| format!("invalid hex value: {s}"))
}

fn fmt_hex_u64(v: u64) -> String {
  format!("0x{v:016x}")
}

fn main() -> Result<()> {
  let cli = Cli::parse();

  let file = std::fs::read(&cli.path)
    .with_context(|| format!("failed to read input file: {}", cli.path.display()))?;

  // Stackmaps can live in different output sections depending on link mode.
  //
  // - For PIE builds we often rewrite the input section to `.data.rel.ro.llvm_stackmaps` so
  //   relocations can be applied safely.
  // - Some linkers/scripts can end up with an output section name without a leading dot.
  const STACKMAP_SECTION_NAMES: [&str; 3] =
    [".data.rel.ro.llvm_stackmaps", ".llvm_stackmaps", "llvm_stackmaps"];
  let section_bytes = if cli.raw_section {
    file.as_slice()
  } else if file.starts_with(b"\x7fELF") {
    let mut last_err: Option<anyhow::Error> = None;
    let mut section = None;
    for name in STACKMAP_SECTION_NAMES {
      match elf::extract_section(&file, name) {
        Ok(s) => {
          section = Some(s);
          break;
        }
        Err(err) => last_err = Some(err),
      }
    }
    let section = section.ok_or_else(|| {
      let names = STACKMAP_SECTION_NAMES.join(", ");
      match last_err {
        Some(err) => {
          anyhow::anyhow!("failed to extract any stackmap section ({names}) from {}: {err}", cli.path.display())
        }
        None => anyhow::anyhow!(
          "failed to extract any stackmap section ({names}) from {}",
          cli.path.display()
        ),
      }
    })?;
    if section.endian != endian::Endian::Little {
      bail!("only little-endian ELF is supported (got {:?})", section.endian);
    }
    section.data
  } else {
    // Common workflow: `llvm-objcopy --dump-section .llvm_stackmaps=out.bin ...`
    file.as_slice()
  };

  let stackmaps =
    parse_all_stackmaps(section_bytes).context("failed to parse stackmap section")?;
  if stackmaps.is_empty() {
    bail!("no StackMap v3 blobs found");
  }

  let filter_addr = match &cli.filter_addr {
    Some(s) => Some(parse_hex_u64(s)?),
    None => None,
  };

  if cli.records {
    if cli.json {
      let output = json_records(&stackmaps, filter_addr)?;
      write_json(&output)?;
    } else {
      print_records(&stackmaps, filter_addr)?;
    }
  } else {
    if cli.json {
      let output = json_summary(&stackmaps);
      write_json(&output)?;
    } else {
      print_summary(&stackmaps)?;
    }
  }

  Ok(())
}

#[derive(Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum JsonOutput {
  Summary(JsonSummary),
  Records(JsonRecords),
}

#[derive(Serialize)]
struct JsonSummary {
  version: u8,
  num_stackmaps: usize,
  num_functions: usize,
  num_constants: usize,
  num_records: usize,
  functions: Vec<JsonFunction>,
}

#[derive(Serialize)]
struct JsonFunction {
  stackmap_index: usize,
  index: usize,
  address: String,
  stack_size: u64,
  record_count: u64,
}

#[derive(Serialize)]
struct JsonRecords {
  version: u8,
  num_stackmaps: usize,
  num_functions: usize,
  num_constants: usize,
  num_records: usize,
  records: Vec<JsonRecord>,
}

#[derive(Serialize)]
struct JsonRecord {
  index: usize,
  stackmap_index: usize,
  function_index: usize,
  function_address: String,
  callsite_address: String,
  patchpoint_id: u64,
  instruction_offset: u32,
  locations: Vec<JsonLocation>,
}

#[derive(Serialize)]
struct JsonLocation {
  kind: String,
  reg: Option<u16>,
  offset: Option<i32>,
  size: u16,
  #[serde(skip_serializing_if = "Option::is_none")]
  value: Option<u64>,
  #[serde(skip_serializing_if = "Option::is_none")]
  const_index: Option<u32>,
}

fn json_summary(stackmaps: &[StackMap]) -> JsonOutput {
  let (num_functions, num_constants, num_records) = totals(stackmaps);

  let mut functions: Vec<JsonFunction> = Vec::with_capacity(num_functions);
  let mut global_index: usize = 0;
  for (stackmap_index, sm) in stackmaps.iter().enumerate() {
    for f in &sm.functions {
      functions.push(JsonFunction {
        stackmap_index,
        index: global_index,
        address: fmt_hex_u64(f.address),
        stack_size: f.stack_size,
        record_count: f.record_count,
      });
      global_index += 1;
    }
  }

  JsonOutput::Summary(JsonSummary {
    version: stackmaps[0].version,
    num_stackmaps: stackmaps.len(),
    num_functions,
    num_constants,
    num_records,
    functions,
  })
}

fn json_records(stackmaps: &[StackMap], filter_addr: Option<u64>) -> Result<JsonOutput> {
  let (num_functions, num_constants, num_records) = totals(stackmaps);
  let mut records = Vec::new();

  let mut global_function_index: usize = 0;
  let mut global_record_index: usize = 0;
  for (stackmap_index, sm) in stackmaps.iter().enumerate() {
    validate_record_counts(sm)?;
    let mut record_index: usize = 0;
    for (function_index, func) in sm.functions.iter().enumerate() {
      let record_count: usize = func
        .record_count
        .try_into()
        .with_context(|| format!("function.record_count does not fit in usize (stackmap[{stackmap_index}] func[{function_index}])"))?;
      for _ in 0..record_count {
        let rec = sm
          .records
          .get(record_index)
          .ok_or_else(|| anyhow::anyhow!("record index out of bounds: {record_index}"))?;

        let callsite_addr = func
          .address
          .checked_add(rec.instruction_offset as u64)
          .context("callsite address overflow")?;
        if filter_addr.map_or(true, |addr| addr == callsite_addr) {
          let locations = rec.locations.iter().map(json_location).collect();

          records.push(JsonRecord {
            index: global_record_index,
            stackmap_index,
            function_index: global_function_index,
            function_address: fmt_hex_u64(func.address),
            callsite_address: fmt_hex_u64(callsite_addr),
            patchpoint_id: rec.patchpoint_id,
            instruction_offset: rec.instruction_offset,
            locations,
          });
        }

        record_index += 1;
        global_record_index += 1;
      }

      global_function_index += 1;
    }
  }

  Ok(JsonOutput::Records(JsonRecords {
    version: stackmaps[0].version,
    num_stackmaps: stackmaps.len(),
    num_functions,
    num_constants,
    num_records,
    records,
  }))
}

fn write_json(output: &JsonOutput) -> Result<()> {
  let stdout = std::io::stdout();
  let mut handle = stdout.lock();
  serde_json::to_writer_pretty(&mut handle, output)?;
  writeln!(&mut handle)?;
  Ok(())
}

fn print_summary(stackmaps: &[StackMap]) -> Result<()> {
  let (num_functions, num_constants, num_records) = totals(stackmaps);
  println!("StackMap v{}", stackmaps[0].version);
  println!("functions: {num_functions}");
  println!("constants: {num_constants}");
  println!("records: {num_records}");
  println!();
  println!("functions:");
  let mut idx: usize = 0;
  for sm in stackmaps {
    for f in &sm.functions {
    println!(
      "  [{idx}] addr={} stack_size={} records={}",
      fmt_hex_u64(f.address),
      f.stack_size,
      f.record_count
    );
      idx += 1;
    }
  }
  Ok(())
}

fn print_records(stackmaps: &[StackMap], filter_addr: Option<u64>) -> Result<()> {
  let mut global_record_index: usize = 0;
  let mut global_function_index: usize = 0;

  for (stackmap_index, sm) in stackmaps.iter().enumerate() {
    validate_record_counts(sm)?;

    let mut record_index: usize = 0;
    for (function_index, func) in sm.functions.iter().enumerate() {
      let record_count: usize = match func.record_count.try_into() {
        Ok(v) => v,
        Err(_) => {
          eprintln!(
            "warning: skipping function stackmap[{stackmap_index}] func[{function_index}] because record_count={} does not fit in usize",
            func.record_count
          );
          global_function_index += 1;
          continue;
        }
      };

      for _ in 0..record_count {
        let Some(rec) = sm.records.get(record_index) else {
          bail!("record index out of bounds: {record_index}");
        };
        let callsite_addr = func
          .address
          .checked_add(rec.instruction_offset as u64)
          .context("callsite address overflow")?;

        if filter_addr.map_or(false, |addr| addr != callsite_addr) {
          record_index += 1;
          global_record_index += 1;
          continue;
        }

        println!(
          "[{global_record_index}] func={global_function_index} callsite={} patchpoint_id={} locations={}",
          fmt_hex_u64(callsite_addr),
          rec.patchpoint_id,
          rec.locations.len()
        );
        for (loc_idx, loc) in rec.locations.iter().enumerate() {
          print_location(loc_idx, loc);
        }

        record_index += 1;
        global_record_index += 1;
      }

      global_function_index += 1;
    }
  }
  Ok(())
}

fn totals(stackmaps: &[StackMap]) -> (usize, usize, usize) {
  let num_functions = stackmaps.iter().map(|m| m.functions.len()).sum();
  let num_constants = stackmaps.iter().map(|m| m.constants.len()).sum();
  let num_records = stackmaps.iter().map(|m| m.records.len()).sum();
  (num_functions, num_constants, num_records)
}

fn validate_record_counts(stackmap: &StackMap) -> Result<()> {
  let mut expected: u64 = 0;
  for f in &stackmap.functions {
    expected = expected.checked_add(f.record_count).context("record_count overflow")?;
  }
  if expected != stackmap.records.len() as u64 {
    bail!(
      "stackmap record count mismatch: functions expect {expected}, section has {}",
      stackmap.records.len()
    );
  }
  Ok(())
}

fn json_location(loc: &Location) -> JsonLocation {
  match *loc {
    Location::Register {
      size,
      dwarf_reg,
      offset,
    } => JsonLocation {
      kind: "register".to_string(),
      reg: Some(dwarf_reg),
      offset: Some(offset),
      size,
      value: None,
      const_index: None,
    },
    Location::Direct {
      size,
      dwarf_reg,
      offset,
    } => JsonLocation {
      kind: "direct".to_string(),
      reg: Some(dwarf_reg),
      offset: Some(offset),
      size,
      value: None,
      const_index: None,
    },
    Location::Indirect {
      size,
      dwarf_reg,
      offset,
    } => JsonLocation {
      kind: "indirect".to_string(),
      reg: Some(dwarf_reg),
      offset: Some(offset),
      size,
      value: None,
      const_index: None,
    },
    Location::Constant { size, value } => JsonLocation {
      kind: "constant".to_string(),
      reg: None,
      offset: None,
      size,
      value: Some(value),
      const_index: None,
    },
    Location::ConstIndex { size, index, value } => JsonLocation {
      kind: "const_index".to_string(),
      reg: None,
      offset: None,
      size,
      value: Some(value),
      const_index: Some(index),
    },
  }
}

fn print_location(idx: usize, loc: &Location) {
  match *loc {
    Location::Register {
      size,
      dwarf_reg,
      offset,
    } => {
      println!("  [{idx}] kind=register reg={dwarf_reg} offset={offset} size={size}");
    }
    Location::Direct {
      size,
      dwarf_reg,
      offset,
    } => {
      println!("  [{idx}] kind=direct reg={dwarf_reg} offset={offset} size={size}");
    }
    Location::Indirect {
      size,
      dwarf_reg,
      offset,
    } => {
      println!("  [{idx}] kind=indirect reg={dwarf_reg} offset={offset} size={size}");
    }
    Location::Constant { size, value } => {
      println!("  [{idx}] kind=constant value={:#x} size={size}", value);
    }
    Location::ConstIndex { size, index, value } => {
      println!(
        "  [{idx}] kind=const_index index={index} value={:#x} size={size}",
        value
      );
    }
  }
}
