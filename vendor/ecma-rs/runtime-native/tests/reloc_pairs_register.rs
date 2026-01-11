use runtime_native::gc_roots::{relocate_reloc_pairs_in_place, RelocPair};
use runtime_native::statepoints::RootSlot;
use stackmap_context::ThreadContext;

#[test]
fn reloc_pairs_can_update_register_roots() {
  let mut ctx = ThreadContext::default();

  // Use DWARF regs that exist on both supported 64-bit targets:
  // - x86_64: 0=RAX, 1=RDX
  // - aarch64: 0=X0, 1=X1
  ctx.set_dwarf_reg_u64(0, 0x1000).unwrap();
  ctx.set_dwarf_reg_u64(1, 0x1010).unwrap();

  let base = RootSlot::Reg { dwarf_reg: 0 };
  let derived = RootSlot::Reg { dwarf_reg: 1 };

  // Ordering mirrors typical statepoint lowering: base==derived pair first, then derived pointer.
  let pairs = [
    RelocPair {
      base_slot: base,
      derived_slot: base,
    },
    RelocPair {
      base_slot: base,
      derived_slot: derived,
    },
  ];

  let mut calls = 0usize;
  relocate_reloc_pairs_in_place(&mut ctx, pairs, |old| {
    calls += 1;
    old + 0x1000
  });

  // Base relocation is deduplicated by slot (only one relocate call).
  assert_eq!(calls, 1);

  assert_eq!(ctx.get_dwarf_reg_u64(0).unwrap(), 0x2000);
  // Derived pointer preserves the (old_derived - old_base) offset (0x10).
  assert_eq!(ctx.get_dwarf_reg_u64(1).unwrap(), 0x2010);
}

