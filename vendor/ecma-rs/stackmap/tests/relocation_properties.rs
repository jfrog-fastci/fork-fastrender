use std::collections::{HashMap, HashSet};

use stackmap::{
  relocate_statepoint_derived_roots, LocationValueAccess, StackMapLocation, StatepointRecordView,
};

#[derive(Debug, Clone, PartialEq, Eq)]
enum MockMachineError {
  LocationSizeMismatch { expected: u16, actual: u16 },
  ConstantDoesNotFitUsize { value: u64 },
  ConstantNotWritable { constant: usize, attempted: usize },
  ConstantIndexUnsupported,
}

#[derive(Default, Debug, Clone)]
struct MockMachine {
  regs: HashMap<u16, usize>,
  mem: HashMap<(u16, i32), usize>,
  direct_mem: HashMap<i64, usize>,
}

impl MockMachine {
  fn ptr_size() -> u16 {
    std::mem::size_of::<usize>() as u16
  }

  fn validate_size(&self, loc: &StackMapLocation) -> Result<(), MockMachineError> {
    let expected = Self::ptr_size();
    let actual = loc.size();
    if expected != actual {
      return Err(MockMachineError::LocationSizeMismatch { expected, actual });
    }
    Ok(())
  }
}

impl LocationValueAccess for MockMachine {
  type Error = MockMachineError;

  fn read_usize(&mut self, loc: &StackMapLocation) -> Result<usize, Self::Error> {
    self.validate_size(loc)?;
    match loc {
      StackMapLocation::Register { dwarf_reg, .. } => Ok(*self.regs.get(dwarf_reg).unwrap_or(&0)),
      StackMapLocation::Indirect {
        dwarf_reg, offset, ..
      } => Ok(*self.mem.get(&(*dwarf_reg, *offset)).unwrap_or(&0)),
      StackMapLocation::Direct { address, .. } => Ok(*self.direct_mem.get(address).unwrap_or(&0)),
      StackMapLocation::Constant { value, .. } => usize::try_from(*value)
        .map_err(|_| MockMachineError::ConstantDoesNotFitUsize { value: *value }),
      StackMapLocation::ConstantIndex { .. } => Err(MockMachineError::ConstantIndexUnsupported),
    }
  }

  fn write_usize(&mut self, loc: &StackMapLocation, value: usize) -> Result<(), Self::Error> {
    self.validate_size(loc)?;
    match loc {
      StackMapLocation::Register { dwarf_reg, .. } => {
        self.regs.insert(*dwarf_reg, value);
        Ok(())
      }
      StackMapLocation::Indirect {
        dwarf_reg, offset, ..
      } => {
        self.mem.insert((*dwarf_reg, *offset), value);
        Ok(())
      }
      StackMapLocation::Direct { address, .. } => {
        self.direct_mem.insert(*address, value);
        Ok(())
      }
      StackMapLocation::Constant { value: constant, .. } => {
        let constant = usize::try_from(*constant)
          .map_err(|_| MockMachineError::ConstantDoesNotFitUsize { value: *constant })?;
        if constant == value {
          // Constants are immutable, but allow a no-op write so that stackmaps can encode null
          // pointers using a constant location and still reuse the same relocation logic.
          Ok(())
        } else {
          Err(MockMachineError::ConstantNotWritable {
            constant,
            attempted: value,
          })
        }
      }
      StackMapLocation::ConstantIndex { .. } => Err(MockMachineError::ConstantIndexUnsupported),
    }
  }
}

#[derive(Clone)]
struct Rng(u64);

impl Rng {
  fn new(seed: u64) -> Self {
    // Avoid the all-zero seed corner case.
    Self(seed ^ 0x9e3779b97f4a7c15)
  }

  fn next_u64(&mut self) -> u64 {
    // xorshift64*
    let mut x = self.0;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    self.0 = x;
    x.wrapping_mul(0x2545F4914F6CDD1D)
  }

  fn gen_bool(&mut self, numerator: u64, denominator: u64) -> bool {
    debug_assert!(numerator <= denominator);
    (self.next_u64() % denominator) < numerator
  }

  fn gen_usize(&mut self, range: std::ops::RangeInclusive<usize>) -> usize {
    let start = *range.start();
    let end = *range.end();
    debug_assert!(start <= end);
    let width = end - start + 1;
    start + (self.next_u64() as usize % width)
  }

  fn choose<'a, T>(&mut self, values: &'a [T]) -> &'a T {
    let idx = self.gen_usize(0..=(values.len() - 1));
    &values[idx]
  }

  fn take_random<T>(&mut self, values: &mut Vec<T>) -> Option<T> {
    if values.is_empty() {
      return None;
    }
    let idx = self.gen_usize(0..=(values.len() - 1));
    Some(values.swap_remove(idx))
  }
}

fn header_constant(value: u64) -> StackMapLocation {
  // LLVM stackmap headers are always 64-bit constants (regardless of pointer width).
  StackMapLocation::Constant { value, size: 8 }
}

fn constant_null_ptr() -> StackMapLocation {
  StackMapLocation::Constant {
    value: 0,
    size: MockMachine::ptr_size(),
  }
}

fn reg(dwarf_reg: u16) -> StackMapLocation {
  StackMapLocation::Register {
    dwarf_reg,
    size: MockMachine::ptr_size(),
  }
}

fn indirect(dwarf_reg: u16, offset: i32) -> StackMapLocation {
  StackMapLocation::Indirect {
    dwarf_reg,
    offset,
    size: MockMachine::ptr_size(),
  }
}

fn direct(address: i64) -> StackMapLocation {
  StackMapLocation::Direct {
    address,
    size: MockMachine::ptr_size(),
  }
}

#[derive(Clone, Debug)]
struct GeneratedCase {
  record_locations: Vec<StackMapLocation>,
  machine: MockMachine,
}

fn generate_case(seed: u64) -> GeneratedCase {
  let mut rng = Rng::new(seed);

  let base_pool: Vec<StackMapLocation> = vec![
    constant_null_ptr(),
    reg(1),
    reg(2),
    reg(3),
    indirect(10, 0),
    indirect(10, 8),
    indirect(11, 0),
    direct(0x10_0000),
  ];

  let mut derived_pool: Vec<StackMapLocation> = vec![
    constant_null_ptr(),
    reg(21),
    reg(22),
    reg(23),
    indirect(30, -8),
    indirect(30, -16),
    indirect(31, 0),
    direct(0x20_0000),
  ];

  // Keep base values small and aligned so we never accidentally wrap to 0 when testing the
  // "base_new = base_old + K" relocation convention.
  let base_value_pool: Vec<usize> = vec![
    0,
    0x1000 + (seed as usize) * 0x100,
    0x2000 + (seed as usize) * 0x100,
    0x3000 + (seed as usize) * 0x100,
  ];

  let unique_pairs = rng.gen_usize(0..=8);

  let deopt_count = rng.gen_usize(0..=2);

  let mut machine = MockMachine::default();
  let mut base_value_by_loc: HashMap<StackMapLocation, usize> = HashMap::new();
  let mut pair_specs: Vec<(StackMapLocation, StackMapLocation)> = Vec::new();

  for _ in 0..unique_pairs {
    let base_loc = rng.choose(&base_pool).clone();

    let base_old = match &base_loc {
      StackMapLocation::Constant { value, .. } => usize::try_from(*value).unwrap(),
      _ => *base_value_by_loc.entry(base_loc.clone()).or_insert_with(|| {
        if rng.gen_bool(1, 5) {
          0
        } else {
          *rng.choose(&base_value_pool[1..])
        }
      }),
    };
    machine.write_usize(&base_loc, base_old).unwrap();

    let (derived_loc, derived_old) = if rng.gen_bool(1, 5) {
      // Alias the derived pointer with the base pointer (base==derived).
      (base_loc.clone(), base_old)
    } else {
      let derived_loc = rng
        .take_random(&mut derived_pool)
        .unwrap_or_else(|| base_loc.clone());

      let derived_old = match &derived_loc {
        StackMapLocation::Constant { value, .. } => usize::try_from(*value).unwrap(),
        _ => {
          if rng.gen_bool(1, 7) {
            // Explicit null derived pointer.
            0
          } else if base_old == 0 {
            // Real-world corner case: pointer arithmetic on null can yield a non-zero derived.
            if rng.gen_bool(1, 2) {
              0
            } else {
              *rng.choose(&base_value_pool[1..])
            }
          } else {
            let delta = rng.gen_usize(0..=0x80);
            if rng.gen_bool(1, 3) {
              base_old.wrapping_add(delta)
            } else {
              base_old.wrapping_sub(delta)
            }
          }
        }
      };

      machine.write_usize(&derived_loc, derived_old).unwrap();
      (derived_loc, derived_old)
    };

    // Ensure aliasing slots are always consistent in the machine.
    machine.write_usize(&derived_loc, derived_old).unwrap();

    pair_specs.push((base_loc, derived_loc));
  }

  // Expand into the final record pairs by duplicating some pairs verbatim.
  let mut record_pairs = Vec::new();
  for pair in &pair_specs {
    record_pairs.push(pair.clone());
    let extra_dupes = rng.gen_usize(0..=2);
    for _ in 0..extra_dupes {
      record_pairs.push(pair.clone());
    }
  }

  // Construct an LLVM-statepoint-compatible record layout:
  // [ calling_conv, flags, deopt_count, deopt_locations..., (base, derived) ... ]
  let mut record_locations = Vec::new();
  record_locations.push(header_constant(0));
  record_locations.push(header_constant(0));
  record_locations.push(header_constant(deopt_count as u64));

  for i in 0..deopt_count {
    // Deopt entries are not accessed by relocation; they just need to be syntactically valid.
    // Mix register and indirect locations to vary the record layout a bit.
    let loc = if i % 2 == 0 {
      reg(100 + i as u16)
    } else {
      indirect(200 + i as u16, i as i32 * 8)
    };
    record_locations.push(loc);
  }

  for (base, derived) in record_pairs {
    record_locations.push(base);
    record_locations.push(derived);
  }

  GeneratedCase {
    record_locations,
    machine,
  }
}

fn relocate_add_k(ptr: usize) -> usize {
  // Chosen to be small enough that our generated base values never wrap.
  ptr.wrapping_add(0x10_0000)
}

fn relocate_hash(ptr: usize) -> usize {
  // A deterministic "relocation" that is:
  // - injective over `usize` (for non-zero inputs), so it won't accidentally map a non-null base
  //   to null.
  // - stable across platforms (the constant is truncated on 32-bit targets).
  ptr
    .wrapping_mul(0x9E37_79B9_7F4A_7C15u64 as usize)
    .rotate_left(7)
}

fn run_relocation_case(case: GeneratedCase, relocate_fn: fn(usize) -> usize) {
  let view = StatepointRecordView::new(&case.record_locations).unwrap();
  let mut machine = case.machine;

  #[derive(Clone, Debug)]
  struct PairSnap {
    base_loc: StackMapLocation,
    derived_loc: StackMapLocation,
    base_old: usize,
    derived_old: usize,
  }

  let mut pair_snaps = Vec::new();
  let mut base_old_by_loc: HashMap<StackMapLocation, usize> = HashMap::new();
  let mut derived_old_by_loc: HashMap<StackMapLocation, usize> = HashMap::new();

  for (base_loc, derived_loc) in view.gc_roots() {
    let base_old = machine.read_usize(base_loc).unwrap();
    let derived_old = machine.read_usize(derived_loc).unwrap();

    match base_old_by_loc.insert(base_loc.clone(), base_old) {
      Some(prev) => assert_eq!(
        prev, base_old,
        "base location must contain a single stable value"
      ),
      None => {}
    }
    match derived_old_by_loc.insert(derived_loc.clone(), derived_old) {
      Some(prev) => assert_eq!(
        prev, derived_old,
        "derived location must contain a single stable value"
      ),
      None => {}
    }

    pair_snaps.push(PairSnap {
      base_loc: base_loc.clone(),
      derived_loc: derived_loc.clone(),
      base_old,
      derived_old,
    });
  }

  let unique_nonnull_base_values: HashSet<usize> =
    base_old_by_loc.values().copied().filter(|v| *v != 0).collect();

  // Expected relocated bases, memoized by old base *value*.
  let mut base_new_by_value: HashMap<usize, usize> = HashMap::new();
  for &base_old in &unique_nonnull_base_values {
    base_new_by_value.insert(base_old, relocate_fn(base_old));
  }

  let mut base_new_by_loc: HashMap<StackMapLocation, usize> = HashMap::new();
  for (base_loc, base_old) in &base_old_by_loc {
    let base_new = if *base_old == 0 {
      0
    } else {
      *base_new_by_value.get(base_old).unwrap()
    };
    base_new_by_loc.insert(base_loc.clone(), base_new);
  }

  let mut derived_new_by_loc: HashMap<StackMapLocation, usize> = HashMap::new();
  for pair in &pair_snaps {
    let base_new = *base_new_by_loc.get(&pair.base_loc).unwrap();
    let derived_new = if pair.base_old == 0 || pair.derived_old == 0 || base_new == 0 {
      0
    } else {
      let delta = pair.derived_old.wrapping_sub(pair.base_old);
      base_new.wrapping_add(delta)
    };

    match derived_new_by_loc.insert(pair.derived_loc.clone(), derived_new) {
      Some(prev) => assert_eq!(
        prev, derived_new,
        "duplicate derived locations must be assigned consistently"
      ),
      None => {}
    }
  }

  // Run relocation with call counting.
  let mut call_counts: HashMap<usize, usize> = HashMap::new();
  relocate_statepoint_derived_roots(&view, &mut machine, |base_old| {
    *call_counts.entry(base_old).or_insert(0) += 1;
    relocate_fn(base_old)
  })
  .unwrap();

  // Invariants:
  // - The base relocator must be called exactly once per unique non-null base *value*.
  // - The base relocator must never be called with a null pointer.
  assert!(
    !call_counts.contains_key(&0),
    "relocation closure must not be invoked for null base pointers"
  );
  assert_eq!(
    call_counts.len(),
    unique_nonnull_base_values.len(),
    "relocation closure must be invoked once per unique base value (not per location)"
  );
  for base_old in unique_nonnull_base_values {
    assert_eq!(
      call_counts.get(&base_old).copied(),
      Some(1),
      "base value {base_old:#x} must be relocated exactly once"
    );
  }

  // Validate final machine state for all touched locations.
  for (loc, base_new) in base_new_by_loc {
    let actual = machine.read_usize(&loc).unwrap();
    assert_eq!(actual, base_new, "base slot did not match expected relocation");
  }
  for (loc, derived_new) in derived_new_by_loc {
    let actual = machine.read_usize(&loc).unwrap();
    assert_eq!(
      actual, derived_new,
      "derived slot did not match expected base+delta recomputation"
    );
  }
}

#[test]
fn relocate_statepoint_derived_roots_property_cases() {
  for seed in 0..200u64 {
    let case = generate_case(seed);
    run_relocation_case(case.clone(), relocate_add_k);
    run_relocation_case(case, relocate_hash);
  }
}

#[test]
fn relocate_statepoint_derived_roots_relocates_each_base_value_once_even_when_duplicated_across_locations(
) {
  let ptr_size = MockMachine::ptr_size();

  // Two distinct base locations contain the same base pointer value.
  let base0 = StackMapLocation::Register {
    dwarf_reg: 1,
    size: ptr_size,
  };
  let base1 = StackMapLocation::Indirect {
    dwarf_reg: 7,
    offset: 8,
    size: ptr_size,
  };
  let derived0 = StackMapLocation::Register {
    dwarf_reg: 2,
    size: ptr_size,
  };
  let derived1 = StackMapLocation::Indirect {
    dwarf_reg: 7,
    offset: 16,
    size: ptr_size,
  };

  let locations = vec![
    header_constant(0),
    header_constant(0),
    header_constant(0), // no deopts
    base0.clone(),
    derived0.clone(),
    base1.clone(),
    derived1.clone(),
  ];
  let view = StatepointRecordView::new(&locations).unwrap();

  let mut machine = MockMachine::default();
  machine.write_usize(&base0, 0x1000).unwrap();
  machine.write_usize(&base1, 0x1000).unwrap();
  machine.write_usize(&derived0, 0x1010).unwrap();
  machine.write_usize(&derived1, 0x1020).unwrap();

  let mut call_counts: HashMap<usize, usize> = HashMap::new();
  relocate_statepoint_derived_roots(&view, &mut machine, |base_old| {
    *call_counts.entry(base_old).or_insert(0) += 1;
    relocate_add_k(base_old)
  })
  .unwrap();

  assert_eq!(
    call_counts.get(&0x1000).copied(),
    Some(1),
    "base relocator must be called once for a duplicated base value"
  );
  assert_eq!(machine.read_usize(&base0).unwrap(), 0x1000 + 0x10_0000);
  assert_eq!(machine.read_usize(&base1).unwrap(), 0x1000 + 0x10_0000);
  assert_eq!(machine.read_usize(&derived0).unwrap(), 0x1010 + 0x10_0000);
  assert_eq!(machine.read_usize(&derived1).unwrap(), 0x1020 + 0x10_0000);
}

#[test]
fn relocate_statepoint_derived_roots_forces_null_when_base_is_null_even_if_derived_is_nonnull() {
  let ptr_size = MockMachine::ptr_size();

  let base = StackMapLocation::Register {
    dwarf_reg: 1,
    size: ptr_size,
  };
  let derived = StackMapLocation::Register {
    dwarf_reg: 2,
    size: ptr_size,
  };

  let locations = vec![
    header_constant(0),
    header_constant(0),
    header_constant(0),
    base.clone(),
    derived.clone(),
  ];
  let view = StatepointRecordView::new(&locations).unwrap();

  let mut machine = MockMachine::default();
  machine.write_usize(&base, 0).unwrap();
  machine.write_usize(&derived, 0x1234).unwrap();

  let mut called = false;
  relocate_statepoint_derived_roots(&view, &mut machine, |base_old| {
    called = true;
    relocate_add_k(base_old)
  })
  .unwrap();

  assert!(!called, "null base pointers must not be passed to the relocator");
  assert_eq!(machine.read_usize(&base).unwrap(), 0);
  assert_eq!(
    machine.read_usize(&derived).unwrap(),
    0,
    "derived pointer must be forced null when base is null"
  );
}
