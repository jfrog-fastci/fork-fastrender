//! Fuzz `stackmap::relocate_statepoint_derived_roots` using synthetic statepoint records.
//!
//! Run (from the repo root) with a hard timeout (via `timeout -k`) and the repo's fuzz wrapper:
//! ```bash
//! # One-time: create a gitignored output corpus directory.
//! mkdir -p vendor/ecma-rs/fuzz/corpus/stackmap_relocate_statepoint
//!
//! timeout -k 10 600 bash vendor/ecma-rs/scripts/cargo_agent.sh fuzz run stackmap_relocate_statepoint \
//!   fuzz/corpus/stackmap_relocate_statepoint -- -max_total_time=10
//! ```
#![no_main]

use libfuzzer_sys::fuzz_target;
use stackmap::{
  relocate_statepoint_derived_roots, LocationValueAccess, StackMapLocation, StatepointRecordView,
};
use std::collections::HashMap;
use std::convert::Infallible;

/// Keep per-input allocations bounded.
const MAX_LOCATIONS: usize = 64;

#[derive(Clone, Copy)]
struct ByteCursor<'a> {
  data: &'a [u8],
  pos: usize,
}

impl<'a> ByteCursor<'a> {
  fn new(data: &'a [u8]) -> Self {
    Self { data, pos: 0 }
  }

  fn read<const N: usize>(&mut self) -> [u8; N] {
    let mut out = [0u8; N];
    for byte in out.iter_mut() {
      if let Some(b) = self.data.get(self.pos) {
        *byte = *b;
      }
      self.pos = self.pos.saturating_add(1);
    }
    out
  }

  fn u8(&mut self) -> u8 {
    self.read::<1>()[0]
  }

  fn u16(&mut self) -> u16 {
    u16::from_le_bytes(self.read::<2>())
  }

  fn u32(&mut self) -> u32 {
    u32::from_le_bytes(self.read::<4>())
  }

  fn i32(&mut self) -> i32 {
    i32::from_le_bytes(self.read::<4>())
  }

  fn u64(&mut self) -> u64 {
    u64::from_le_bytes(self.read::<8>())
  }

  fn i64(&mut self) -> i64 {
    i64::from_le_bytes(self.read::<8>())
  }
}

fn make_location(cursor: &mut ByteCursor<'_>, pool: &[StackMapLocation]) -> StackMapLocation {
  let tag = cursor.u8() % 5;
  let new_loc = match tag {
    0 => StackMapLocation::Register {
      dwarf_reg: cursor.u16(),
      size: cursor.u16(),
    },
    1 => StackMapLocation::Direct {
      address: cursor.i64(),
      size: cursor.u16(),
    },
    2 => StackMapLocation::Indirect {
      dwarf_reg: cursor.u16(),
      offset: cursor.i32(),
      size: cursor.u16(),
    },
    3 => StackMapLocation::Constant {
      value: cursor.u64(),
      size: cursor.u16(),
    },
    _ => StackMapLocation::ConstantIndex {
      index: cursor.u32(),
      size: cursor.u16(),
    },
  };

  // Increase the chance of repeated locations to exercise snapshotting and duplicate write paths.
  if pool.is_empty() {
    return new_loc;
  }
  let reuse = cursor.u8();
  if reuse & 1 == 0 {
    pool[reuse as usize % pool.len()].clone()
  } else {
    new_loc
  }
}

#[derive(Default)]
struct MockAccess {
  values: HashMap<StackMapLocation, usize>,
}

impl MockAccess {
  fn seed_value(loc: &StackMapLocation) -> usize {
    match loc {
      StackMapLocation::Register { dwarf_reg, .. } => *dwarf_reg as usize,
      StackMapLocation::Direct { address, .. } => *address as usize,
      StackMapLocation::Indirect {
        dwarf_reg, offset, ..
      } => ((*dwarf_reg as usize) << 32) ^ (*offset as u32 as usize),
      StackMapLocation::Constant { value, .. } => *value as usize,
      StackMapLocation::ConstantIndex { index, .. } => *index as usize,
    }
  }
}

impl LocationValueAccess for MockAccess {
  type Error = Infallible;

  fn read_usize(&mut self, loc: &StackMapLocation) -> Result<usize, Self::Error> {
    if let Some(v) = self.values.get(loc) {
      return Ok(*v);
    }
    let v = Self::seed_value(loc);
    self.values.insert(loc.clone(), v);
    Ok(v)
  }

  fn write_usize(&mut self, loc: &StackMapLocation, value: usize) -> Result<(), Self::Error> {
    self.values.insert(loc.clone(), value);
    Ok(())
  }
}

fuzz_target!(|data: &[u8]| {
  let mut cursor = ByteCursor::new(data);

  let calling_convention = cursor.u64();
  let flags = cursor.u64();

  let desired_deopt = (cursor.u8() as usize).min(MAX_LOCATIONS.saturating_sub(3));
  let max_roots = (MAX_LOCATIONS - 3 - desired_deopt) / 2;
  let root_count = if max_roots == 0 {
    0
  } else {
    (cursor.u8() as usize) % (max_roots + 1)
  };

  let total_locations = 3 + desired_deopt + 2 * root_count;
  let mut locations = Vec::with_capacity(total_locations);

  // Force a well-formed header so we exercise relocation logic frequently.
  locations.push(StackMapLocation::Constant {
    value: calling_convention,
    size: 8,
  });
  locations.push(StackMapLocation::Constant { value: flags, size: 8 });
  locations.push(StackMapLocation::Constant {
    value: desired_deopt as u64,
    size: 8,
  });

  for _ in 0..(total_locations - 3) {
    let loc = make_location(&mut cursor, &locations);
    locations.push(loc);
  }

  let Ok(view) = StatepointRecordView::new(&locations) else {
    return;
  };

  let mut access = MockAccess::default();
  let relocate_delta = cursor.u64() as usize;
  let _ = relocate_statepoint_derived_roots(&view, &mut access, |base| {
    // Occasionally return 0 to exercise the "relocator returned null" edge case.
    if (base ^ relocate_delta) & 0x1f == 0 {
      0
    } else {
      base.wrapping_add(relocate_delta | 1)
    }
  });
});
