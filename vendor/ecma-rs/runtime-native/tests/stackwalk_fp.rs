#[cfg(target_arch = "x86_64")]
use runtime_native::{walk_gc_roots_from_fp, StackMaps};

#[cfg(target_arch = "x86_64")]
#[test]
fn synthetic_stack_enumerates_roots_from_stackmaps() {
    use runtime_native::stackmaps::Location;
    use runtime_native::statepoints::StatepointRecord;

    let stackmaps =
        StackMaps::parse(include_bytes!("fixtures/statepoint_x86_64.bin")).expect("parse stackmaps");

    // Pick the first callsite record (BTreeMap iteration is sorted).
    let (callsite_ra, callsite) = stackmaps.iter().next().expect("non-empty");
    let statepoint = StatepointRecord::new(callsite.record).expect("decode statepoint layout");

    // Fake stack memory.
    let mut stack = vec![0u8; 512];
    let base = stack.as_mut_ptr() as usize;

    // Two frames:
    //   [runtime frame (start_fp)] -> [managed caller frame] -> null
    //
    // We put them at increasing addresses to simulate a downward-growing stack
    // where older frames are at higher addresses.
    let start_fp = align_up(base + 128, 8);
    let caller_fp = align_up(base + 256, 8);

    unsafe {
        // runtime frame points at the caller frame.
        write_u64(start_fp + 0, caller_fp as u64);
        write_u64(start_fp + 8, callsite_ra);

        // caller frame is terminal.
        write_u64(caller_fp + 0, 0);
        write_u64(caller_fp + 8, 0);
    }

    // Compute caller SP using the same formula as the walker (x86_64):
    //   caller_sp = caller_fp - (stack_size - FP_RECORD_SIZE)
    // FP_RECORD_SIZE=8 on x86_64.
    let caller_sp = (caller_fp as u64) - (callsite.stack_size - 8);

    let mut expected_slots: Vec<usize> = Vec::new();
    for pair in statepoint.gc_pairs() {
        for loc in [pair.base, pair.derived] {
            match loc {
                Location::Indirect { dwarf_reg, offset, .. } => {
                    assert_eq!(*dwarf_reg, 7, "fixture roots must be [SP + off]");
                    let slot_addr = add_signed_u64(caller_sp, *offset).expect("slot addr");
                    expected_slots.push(slot_addr as usize);
                }
                other => panic!("unexpected root location kind in fixture: {other:?}"),
            }
        }
    }
    expected_slots.sort_unstable();
    expected_slots.dedup();

    let mut visited: Vec<usize> = Vec::new();
    unsafe {
        walk_gc_roots_from_fp(start_fp as u64, &stackmaps, |slot| {
            visited.push(slot as usize);
        })
        .expect("walk");
    }

    visited.sort_unstable();
    assert_eq!(visited, expected_slots);
    assert_eq!(visited.len(), expected_slots.len());
}

#[cfg(target_arch = "x86_64")]
fn align_up(v: usize, align: usize) -> usize {
    (v + (align - 1)) & !(align - 1)
}

#[cfg(target_arch = "x86_64")]
unsafe fn write_u64(addr: usize, val: u64) {
    (addr as *mut u64).write_unaligned(val);
}

#[cfg(target_arch = "x86_64")]
fn add_signed_u64(base: u64, offset: i32) -> Option<u64> {
    if offset >= 0 {
        base.checked_add(offset as u64)
    } else {
        base.checked_sub((-offset) as u64)
    }
}
