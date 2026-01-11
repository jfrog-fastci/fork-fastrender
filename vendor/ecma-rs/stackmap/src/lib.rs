use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum StackMapLocation {
  Register {
    dwarf_reg: u16,
    size: u16,
  },
  Direct {
    address: i64,
    size: u16,
  },
  Indirect {
    dwarf_reg: u16,
    offset: i32,
    size: u16,
  },
  Constant {
    value: u64,
    size: u16,
  },
  ConstantIndex {
    index: u32,
    size: u16,
  },
}

impl StackMapLocation {
  pub fn size(&self) -> u16 {
    match self {
      StackMapLocation::Register { size, .. } => *size,
      StackMapLocation::Direct { size, .. } => *size,
      StackMapLocation::Indirect { size, .. } => *size,
      StackMapLocation::Constant { size, .. } => *size,
      StackMapLocation::ConstantIndex { size, .. } => *size,
    }
  }

  pub fn constant_value(&self) -> Option<u64> {
    match self {
      StackMapLocation::Constant { value, .. } => Some(*value),
      _ => None,
    }
  }
}

#[derive(Debug, thiserror::Error)]
pub enum StatepointRecordError {
  #[error("statepoint stackmap record must contain at least 3 locations (found {locations_len})")]
  TooFewLocations { locations_len: usize },

  #[error(
    "statepoint stackmap record header location #{header_index} must be Constant (found {found_kind})"
  )]
  HeaderNotConstant {
    header_index: usize,
    found_kind: &'static str,
  },

  #[error("statepoint stackmap record deopt count {deopt_count} does not fit usize")]
  DeoptCountTooLarge { deopt_count: u64 },

  #[error(
    "statepoint stackmap record declares {deopt_count} deopt locations but only {remaining_locations} locations remain after the 3-entry header"
  )]
  DeoptCountExceedsLocations {
    deopt_count: usize,
    remaining_locations: usize,
  },

  #[error(
    "statepoint stackmap record has {gc_locations_len} trailing locations after header+deopt; expected an even number for (base, derived) pairs"
  )]
  OddGcLocationCount { gc_locations_len: usize },
}

pub struct StatepointRecordView<'a> {
  pub calling_convention: u64,
  pub flags: u64,
  pub deopt_locations: &'a [StackMapLocation],
  gc_locations: &'a [StackMapLocation],
}

impl<'a> StatepointRecordView<'a> {
  pub fn new(locations: &'a [StackMapLocation]) -> Result<Self, StatepointRecordError> {
    if locations.len() < 3 {
      return Err(StatepointRecordError::TooFewLocations {
        locations_len: locations.len(),
      });
    }

    let calling_convention = header_constant(locations, 0)?;
    let flags = header_constant(locations, 1)?;
    let deopt_count_u64 = header_constant(locations, 2)?;

    let deopt_count =
      usize::try_from(deopt_count_u64).map_err(|_| StatepointRecordError::DeoptCountTooLarge {
        deopt_count: deopt_count_u64,
      })?;

    let remaining_locations = locations.len() - 3;
    if deopt_count > remaining_locations {
      return Err(StatepointRecordError::DeoptCountExceedsLocations {
        deopt_count,
        remaining_locations,
      });
    }

    let deopt_start = 3;
    let deopt_end = deopt_start + deopt_count;
    let deopt_locations = &locations[deopt_start..deopt_end];
    let gc_locations = &locations[deopt_end..];

    if gc_locations.len() % 2 != 0 {
      return Err(StatepointRecordError::OddGcLocationCount {
        gc_locations_len: gc_locations.len(),
      });
    }

    Ok(Self {
      calling_convention,
      flags,
      deopt_locations,
      gc_locations,
    })
  }

  pub fn gc_roots(
    &self,
  ) -> impl Iterator<Item = (&'a StackMapLocation, &'a StackMapLocation)> + 'a {
    self.gc_locations.chunks_exact(2).map(|pair| {
      let [base, derived] = pair else {
        unreachable!("chunks_exact(2) never yields non-2-sized chunks");
      };
      (base, derived)
    })
  }

  pub fn gc_root_count(&self) -> usize {
    self.gc_locations.len() / 2
  }
}

fn header_constant(
  locations: &[StackMapLocation],
  header_index: usize,
) -> Result<u64, StatepointRecordError> {
  let loc = &locations[header_index];
  loc
    .constant_value()
    .ok_or_else(|| StatepointRecordError::HeaderNotConstant {
      header_index: header_index + 1,
      found_kind: loc.kind_name(),
    })
}

impl StackMapLocation {
  fn kind_name(&self) -> &'static str {
    match self {
      StackMapLocation::Register { .. } => "Register",
      StackMapLocation::Direct { .. } => "Direct",
      StackMapLocation::Indirect { .. } => "Indirect",
      StackMapLocation::Constant { .. } => "Constant",
      StackMapLocation::ConstantIndex { .. } => "ConstantIndex",
    }
  }
}

pub trait LocationValueAccess {
  type Error;

  fn read_usize(&mut self, loc: &StackMapLocation) -> Result<usize, Self::Error>;
  fn write_usize(&mut self, loc: &StackMapLocation, value: usize) -> Result<(), Self::Error>;
}

pub fn relocate_statepoint_derived_roots<A>(
  record: &StatepointRecordView<'_>,
  access: &mut A,
  mut relocate_base: impl FnMut(usize) -> usize,
) -> Result<(), A::Error>
where
  A: LocationValueAccess,
{
  let mut updates = Vec::with_capacity(record.gc_root_count());

  for (base_loc, derived_loc) in record.gc_roots() {
    let base = access.read_usize(base_loc)?;
    let derived = access.read_usize(derived_loc)?;

    let offset = (derived as isize).wrapping_sub(base as isize);
    let new_base = relocate_base(base);
    let new_derived = (new_base as isize).wrapping_add(offset) as usize;

    updates.push((derived_loc, new_derived));
  }

  for (derived_loc, new_derived) in updates {
    access.write_usize(derived_loc, new_derived)?;
  }

  Ok(())
}

#[cfg(test)]
mod tests {
  use std::collections::HashMap;
  use std::process::Command;

  use tempfile::tempdir;

  use super::*;

  #[test]
  fn statepoint_record_view_decodes_header_deopt_and_roots() {
    let locs = vec![
      StackMapLocation::Constant { value: 8, size: 8 },
      StackMapLocation::Constant { value: 1, size: 8 },
      StackMapLocation::Constant { value: 2, size: 8 },
      StackMapLocation::Register {
        dwarf_reg: 42,
        size: 8,
      },
      StackMapLocation::Constant {
        value: 123,
        size: 8,
      },
      StackMapLocation::Indirect {
        dwarf_reg: 7,
        offset: 8,
        size: 8,
      },
      StackMapLocation::Indirect {
        dwarf_reg: 7,
        offset: 8,
        size: 8,
      },
    ];

    let view = StatepointRecordView::new(&locs).unwrap();
    assert_eq!(view.calling_convention, 8);
    assert_eq!(view.flags, 1);
    assert_eq!(view.deopt_locations.len(), 2);
    assert_eq!(view.gc_root_count(), 1);

    let mut roots = view.gc_roots();
    let (base, derived) = roots.next().unwrap();
    assert_eq!(
      (base, derived),
      (&locs[5], &locs[6]),
      "gc_roots must point at trailing (base, derived) pairs"
    );
    assert!(roots.next().is_none());
  }

  #[test]
  fn statepoint_record_view_validates_location_count_and_pairing() {
    assert!(matches!(
      StatepointRecordView::new(&[]).unwrap_err(),
      StatepointRecordError::TooFewLocations { .. }
    ));

    let locs = vec![
      StackMapLocation::Constant { value: 0, size: 8 },
      StackMapLocation::Constant { value: 0, size: 8 },
      StackMapLocation::Constant { value: 0, size: 8 },
      StackMapLocation::Indirect {
        dwarf_reg: 7,
        offset: 8,
        size: 8,
      },
    ];
    assert!(matches!(
      StatepointRecordView::new(&locs).unwrap_err(),
      StatepointRecordError::OddGcLocationCount { .. }
    ));
  }

  #[test]
  fn relocate_statepoint_derived_roots_uses_base_and_derived_semantics() {
    let locs = vec![
      StackMapLocation::Constant { value: 0, size: 8 },
      StackMapLocation::Constant { value: 0, size: 8 },
      StackMapLocation::Constant { value: 0, size: 8 },
      // Root #0: base=B, derived=B (same location)
      StackMapLocation::Indirect {
        dwarf_reg: 7,
        offset: 8,
        size: 8,
      },
      StackMapLocation::Indirect {
        dwarf_reg: 7,
        offset: 8,
        size: 8,
      },
      // Root #1: base=B, derived=D (interior pointer)
      StackMapLocation::Indirect {
        dwarf_reg: 7,
        offset: 8,
        size: 8,
      },
      StackMapLocation::Indirect {
        dwarf_reg: 7,
        offset: 16,
        size: 8,
      },
    ];

    let view = StatepointRecordView::new(&locs).unwrap();

    #[derive(Default)]
    struct MapAccess {
      values: HashMap<StackMapLocation, usize>,
    }

    impl LocationValueAccess for MapAccess {
      type Error = ();

      fn read_usize(&mut self, loc: &StackMapLocation) -> Result<usize, Self::Error> {
        Ok(*self.values.get(loc).unwrap_or(&0))
      }

      fn write_usize(&mut self, loc: &StackMapLocation, value: usize) -> Result<(), Self::Error> {
        self.values.insert(loc.clone(), value);
        Ok(())
      }
    }

    let base = StackMapLocation::Indirect {
      dwarf_reg: 7,
      offset: 8,
      size: 8,
    };
    let derived = StackMapLocation::Indirect {
      dwarf_reg: 7,
      offset: 16,
      size: 8,
    };

    let mut access = MapAccess::default();
    access.values.insert(base.clone(), 0x1000);
    access.values.insert(derived.clone(), 0x1020);

    relocate_statepoint_derived_roots(&view, &mut access, |ptr| match ptr {
      0x1000 => 0x2000,
      other => other,
    })
    .unwrap();

    assert_eq!(access.values[&base], 0x2000, "B should be relocated");
    assert_eq!(
      access.values[&derived], 0x2020,
      "D should be recomputed from relocated base + (D - B) offset"
    );
  }

  #[test]
  fn golden_statepoint_record_layout_from_llvm_readobj() {
    let llc = find_llvm_tool(&["llc-18", "llc"]).expect("llc not found");
    let llvm_readobj =
      find_llvm_tool(&["llvm-readobj-18", "llvm-readobj"]).expect("llvm-readobj not found");

    let dir = tempdir().unwrap();
    let ll_path = dir.path().join("test.ll");
    let obj_path = dir.path().join("test.o");

    std::fs::write(
      &ll_path,
      r#"
; ModuleID = 'statepoint_fastcc_gc'
source_filename = "statepoint_fastcc_gc"

declare token @llvm.experimental.gc.statepoint.p0(i64, i32, ptr, i32, i32, ...)
declare ptr addrspace(1) @llvm.experimental.gc.relocate.p1(token, i32, i32)

declare void @callee(i64)
declare void @use(ptr addrspace(1))

define void @test(ptr addrspace(1) %p, i64 %x) gc "coreclr" {
entry:
  %safepoint = call fastcc token (i64, i32, ptr, i32, i32, ...) @llvm.experimental.gc.statepoint.p0(i64 0, i32 0, ptr elementtype(void (i64)) @callee, i32 1, i32 1, i64 %x, i32 0, i32 0) [ "deopt"(i64 %x, i64 123), "gc-live"(ptr addrspace(1) %p) ]
  %p.relocated = call ptr addrspace(1) @llvm.experimental.gc.relocate.p1(token %safepoint, i32 0, i32 0)
  call void @use(ptr addrspace(1) %p.relocated)
  ret void
}
"#,
    )
    .unwrap();

    let llc_out = Command::new(llc)
      .args(["-O0", "-filetype=obj"])
      .arg(&ll_path)
      .arg("-o")
      .arg(&obj_path)
      .output()
      .unwrap();
    assert!(
      llc_out.status.success(),
      "llc failed:\nstdout:\n{}\nstderr:\n{}",
      String::from_utf8_lossy(&llc_out.stdout),
      String::from_utf8_lossy(&llc_out.stderr)
    );

    let readobj_out = Command::new(llvm_readobj)
      .args(["--stackmap"])
      .arg(&obj_path)
      .output()
      .unwrap();
    assert!(
      readobj_out.status.success(),
      "llvm-readobj failed:\nstdout:\n{}\nstderr:\n{}",
      String::from_utf8_lossy(&readobj_out.stdout),
      String::from_utf8_lossy(&readobj_out.stderr)
    );

    let output = String::from_utf8(readobj_out.stdout).unwrap();
    let locations = parse_first_stackmap_record_locations(&output);

    let view = StatepointRecordView::new(&locations).unwrap();
    assert_eq!(view.calling_convention, 8);
    assert_eq!(view.flags, 1);
    assert_eq!(view.deopt_locations.len(), 2);
    assert_eq!(view.gc_root_count(), 1);
    assert_eq!(
      locations.len(),
      3 + view.deopt_locations.len() + view.gc_root_count() * 2
    );
  }

  fn find_llvm_tool(candidates: &[&'static str]) -> Option<&'static str> {
    for &candidate in candidates {
      match Command::new(candidate).arg("--version").output() {
        Ok(output) if output.status.success() => return Some(candidate),
        _ => {}
      }
    }
    None
  }

  fn parse_first_stackmap_record_locations(output: &str) -> Vec<StackMapLocation> {
    let mut lines = output.lines();
    while let Some(line) = lines.next() {
      if line.trim().ends_with("locations:") {
        break;
      }
    }

    let mut locations = Vec::new();
    for line in lines {
      let line = line.trim();
      if line.contains("live-outs:") {
        break;
      }
      if !line.starts_with('#') {
        continue;
      }
      locations.push(parse_stackmap_location(line));
    }
    locations
  }

  fn parse_stackmap_location(line: &str) -> StackMapLocation {
    let mut parts = line.splitn(2, ':');
    let _idx = parts.next().expect("location index");
    let rest = parts.next().expect("location contents").trim();

    let (kind, rest) = rest
      .split_once(' ')
      .unwrap_or_else(|| panic!("unexpected location line: {line}"));

    let size = parse_size(rest).unwrap_or_else(|| panic!("missing size in location line: {line}"));

    match kind {
      "Constant" => {
        let value_str = rest.split_once(',').map(|(v, _)| v.trim()).unwrap_or(rest);
        let value_i64: i64 = value_str
          .parse()
          .unwrap_or_else(|_| panic!("failed to parse constant value from location line: {line}"));
        StackMapLocation::Constant {
          value: value_i64 as u64,
          size,
        }
      }
      "Indirect" => {
        let bracketed = rest
          .split_once('[')
          .and_then(|(_, r)| r.split_once(']'))
          .map(|(inside, _)| inside)
          .unwrap_or_else(|| panic!("failed to parse Indirect payload from location line: {line}"));

        let bracketed = bracketed.trim();
        let bracketed = bracketed
          .strip_prefix("R#")
          .unwrap_or_else(|| panic!("expected Indirect to start with R#: {line}"));
        let (reg_str, offset_str) = bracketed
          .split_once('+')
          .unwrap_or_else(|| panic!("expected Indirect to contain '+': {line}"));
        let dwarf_reg: u16 = reg_str.trim().parse().unwrap_or_else(|_| {
          panic!("failed to parse dwarf reg from Indirect location line: {line}")
        });
        let offset: i32 = offset_str
          .trim()
          .parse()
          .unwrap_or_else(|_| panic!("failed to parse offset from Indirect location line: {line}"));

        StackMapLocation::Indirect {
          dwarf_reg,
          offset,
          size,
        }
      }
      "Register" => {
        let reg_str = rest
          .trim()
          .strip_prefix("R#")
          .and_then(|r| r.split_once(',').map(|(r, _)| r))
          .unwrap_or_else(|| panic!("failed to parse Register payload: {line}"));
        let dwarf_reg: u16 = reg_str.trim().parse().unwrap_or_else(|_| {
          panic!("failed to parse dwarf reg from Register location line: {line}")
        });
        StackMapLocation::Register { dwarf_reg, size }
      }
      other => panic!("unsupported stackmap location kind {other} in line: {line}"),
    }
  }

  fn parse_size(s: &str) -> Option<u16> {
    let (_, size_str) = s.split_once("size:")?;
    let size_str = size_str.trim();
    size_str.parse().ok()
  }
}

impl fmt::Debug for StatepointRecordView<'_> {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("StatepointRecordView")
      .field("calling_convention", &self.calling_convention)
      .field("flags", &self.flags)
      .field("deopt_locations", &self.deopt_locations)
      .field("gc_locations", &self.gc_locations)
      .finish()
  }
}
