#![no_main]

use libfuzzer_sys::fuzz_target;
use stackmap::{relocate_statepoint_derived_roots, LocationValueAccess, StackMapLocation, StatepointRecordView};
use std::collections::HashMap;

const MAX_STACKMAP_BYTES: usize = 256 * 1024;
const MAX_RECORDS_TO_TRY: usize = 8;
const MAX_LOCATIONS_PER_RECORD: usize = 4096;
const MAX_GC_ROOT_PAIRS: usize = 1024;

#[derive(Clone, Copy)]
struct ByteCursor<'a> {
  data: &'a [u8],
  pos: usize,
}

impl<'a> ByteCursor<'a> {
  fn new(data: &'a [u8]) -> Self {
    Self { data, pos: 0 }
  }

  fn next_u8(&mut self) -> u8 {
    let b = self.data.get(self.pos).copied().unwrap_or(0);
    self.pos = self.pos.saturating_add(1);
    b
  }

  fn next_bool(&mut self) -> bool {
    (self.next_u8() & 1) != 0
  }

  fn next_u64(&mut self) -> u64 {
    let mut bytes = [0u8; 8];
    for b in &mut bytes {
      *b = self.next_u8();
    }
    u64::from_le_bytes(bytes)
  }

  fn next_usize(&mut self) -> usize {
    self.next_u64() as usize
  }

  fn next_usize_bounded(&mut self, max_exclusive: usize) -> usize {
    if max_exclusive == 0 {
      return 0;
    }
    self.next_usize() % max_exclusive
  }
}

fn convert_location(loc: &llvm_stackmaps::Location) -> StackMapLocation {
  match loc {
    llvm_stackmaps::Location::Register { dwarf_reg, size } => StackMapLocation::Register {
      dwarf_reg: *dwarf_reg,
      size: *size,
    },
    llvm_stackmaps::Location::Direct {
      dwarf_reg,
      offset,
      size,
    } => {
      // `stackmap::StackMapLocation::Direct` models an absolute address. For fuzzing, collapse
      // the (reg, offset) pair into a stable synthetic address so different Direct locations remain
      // distinct but don't require a full register file model.
      let address = ((*dwarf_reg as i64) << 32).wrapping_add(*offset as i64);
      StackMapLocation::Direct {
        address,
        size: *size,
      }
    }
    llvm_stackmaps::Location::Indirect {
      dwarf_reg,
      offset,
      size,
    } => StackMapLocation::Indirect {
      dwarf_reg: *dwarf_reg,
      offset: *offset,
      size: *size,
    },
    llvm_stackmaps::Location::Constant { value, size } => StackMapLocation::Constant {
      value: *value as u64,
      size: *size,
    },
    llvm_stackmaps::Location::ConstantIndex { index, size, .. } => StackMapLocation::ConstantIndex {
      index: *index,
      size: *size,
    },
  }
}

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

fuzz_target!(|data: &[u8]| {
  let data = if data.len() > MAX_STACKMAP_BYTES {
    &data[..MAX_STACKMAP_BYTES]
  } else {
    data
  };

  let Ok(stackmaps) = llvm_stackmaps::StackMaps::parse(data) else {
    return;
  };

  if stackmaps.records.is_empty() {
    return;
  }

  let mut cursor = ByteCursor::new(data);
  let tries = (cursor.next_u8() as usize % MAX_RECORDS_TO_TRY).max(1);
  for _ in 0..tries {
    let idx = cursor.next_usize_bounded(stackmaps.records.len());
    let record = &stackmaps.records[idx];

    // Only exercise relocation on records that look like statepoints.
    if llvm_stackmaps::StatepointRecordView::decode(record).is_none() {
      continue;
    }

    if record.locations.len() > MAX_LOCATIONS_PER_RECORD {
      continue;
    }

    let locations: Vec<StackMapLocation> = record.locations().iter().map(convert_location).collect();
    let Ok(view) = StatepointRecordView::new(&locations) else {
      continue;
    };

    if view.gc_root_count() > MAX_GC_ROOT_PAIRS {
      continue;
    }

    let mut access = MapAccess::default();

    // Initialize a synthetic frame with random pointer values for each referenced location.
    for (base_loc, derived_loc) in view.gc_roots() {
      for loc in [base_loc, derived_loc] {
        if access.values.contains_key(loc) {
          continue;
        }
        let mut v = cursor.next_usize();
        // Bias towards null pointers to exercise null/zero handling.
        if cursor.next_bool() {
          v = 0;
        }
        access.values.insert(loc.clone(), v);
      }
    }

    // Snapshot the old values per root pair so we can validate relocation semantics.
    #[derive(Clone)]
    struct PairSnapshot {
      base: StackMapLocation,
      derived: StackMapLocation,
      base_old: usize,
      derived_old: usize,
    }

    let mut pairs: Vec<PairSnapshot> = Vec::with_capacity(view.gc_root_count());
    for (base_loc, derived_loc) in view.gc_roots() {
      let base_old = *access.values.get(base_loc).unwrap_or(&0);
      let derived_old = *access.values.get(derived_loc).unwrap_or(&0);
      pairs.push(PairSnapshot {
        base: base_loc.clone(),
        derived: derived_loc.clone(),
        base_old,
        derived_old,
      });
    }

    let relocate_delta = cursor.next_usize();
    let force_null_mask = if cursor.next_bool() {
      cursor.next_usize().max(1)
    } else {
      0
    };

    let mut relocate_base = |ptr: usize| -> usize {
      if ptr == 0 {
        return 0;
      }
      // Occasionally force a relocation-to-null to stress null handling.
      if force_null_mask != 0 && (ptr & force_null_mask) == 0 {
        return 0;
      }
      ptr.wrapping_add(relocate_delta)
    };

    let _ = relocate_statepoint_derived_roots(&view, &mut access, &mut relocate_base);

    // Validate the relocation invariants. If these assertions fire, we found a bug in the derived
    // root relocation logic.
    for pair in pairs {
      let base_new_expected = if pair.base_old == 0 {
        0
      } else {
        relocate_base(pair.base_old)
      };
      let derived_new_expected = if pair.base_old == 0 || pair.derived_old == 0 || base_new_expected == 0 {
        0
      } else {
        let delta = pair.derived_old.wrapping_sub(pair.base_old);
        base_new_expected.wrapping_add(delta)
      };

      let base_new_actual = *access.values.get(&pair.base).unwrap_or(&0);
      let derived_new_actual = *access.values.get(&pair.derived).unwrap_or(&0);

      assert_eq!(base_new_actual, base_new_expected);
      assert_eq!(derived_new_actual, derived_new_expected);
    }
  }
});

