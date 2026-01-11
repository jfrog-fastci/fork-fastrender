#![cfg(all(target_os = "linux", target_arch = "x86_64"))]

use runtime_native::gc_roots::{relocate_reloc_pairs_in_place, RelocPair};
use runtime_native::stackmaps::{Location, StackMapRecord};
use runtime_native::statepoints::{eval_location, RegFile, RootSlot, StatepointRecord, X86_64_DWARF_REG_SP};
use stackmap_context::ThreadContext;

#[derive(Clone, Copy)]
struct Regs {
  sp: u64,
}

impl RegFile for Regs {
  fn get(&self, dwarf_reg: u16) -> Option<u64> {
    match dwarf_reg {
      X86_64_DWARF_REG_SP => Some(self.sp),
      _ => None,
    }
  }
}

#[test]
fn derived_pointer_delta_is_preserved_after_base_relocation() {
  // A synthetic statepoint record containing exactly one (base, derived) pair:
  //   base    = Indirect [SP + 0]
  //   derived = Indirect [SP + 8]
  //
  // This models how LLVM encodes interior pointers in `.llvm_stackmaps`.
  let record = StackMapRecord {
    patchpoint_id: 0xabcdef00,
    instruction_offset: 0x10,
    locations: vec![
      // 3 constant header locations (callconv, flags, deopt_count=0).
      Location::Constant { size: 8, value: 0 },
      Location::Constant { size: 8, value: 0 },
      Location::Constant { size: 8, value: 0 },
      // (base, derived) GC relocation pair.
      Location::Indirect {
        size: 8,
        dwarf_reg: X86_64_DWARF_REG_SP,
        offset: 0,
      },
      Location::Indirect {
        size: 8,
        dwarf_reg: X86_64_DWARF_REG_SP,
        offset: 8,
      },
    ],
    live_outs: Vec::new(),
  };

  let sp = StatepointRecord::new(&record).expect("decode statepoint record");
  assert_eq!(sp.gc_pair_count(), 1);
  let pair = &sp.gc_pairs()[0];
  assert_ne!(pair.base, pair.derived, "test invariant: base != derived");

  // Fake stack memory for the reported Indirect slots.
  let base_val: usize = 0x1000;
  let delta: usize = 0x30;
  let mut frame_slots: [usize; 2] = [base_val, base_val + delta];
  let regs = Regs {
    sp: frame_slots.as_mut_ptr() as u64,
  };

  let base_slot = eval_location(&pair.base, &regs).expect("eval base location");
  let derived_slot = eval_location(&pair.derived, &regs).expect("eval derived location");

  let (RootSlot::StackAddr(base_addr), RootSlot::StackAddr(derived_addr)) = (base_slot, derived_slot) else {
    panic!("test invariant: expected Indirect locations to evaluate to stack addresses");
  };

  assert_eq!(
    base_addr as usize,
    (&mut frame_slots[0] as *mut usize) as usize
  );
  assert_eq!(
    derived_addr as usize,
    (&mut frame_slots[1] as *mut usize) as usize
  );

  let mut ctx = ThreadContext::default();
  relocate_reloc_pairs_in_place(
    &mut ctx,
    [RelocPair {
      base_slot,
      derived_slot,
    }],
    |old_base| {
      assert_eq!(old_base, base_val);
      0x5000
    },
  );

  assert_eq!(frame_slots[0], 0x5000);
  assert_eq!(frame_slots[1], 0x5000 + delta);
}

