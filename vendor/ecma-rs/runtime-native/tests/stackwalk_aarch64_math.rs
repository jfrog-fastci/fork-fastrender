use runtime_native::arch::aarch64::RegContext;
use runtime_native::scan::compute_sp_aarch64;
use runtime_native::stackmaps::Location;
use runtime_native::statepoints::{eval_location, RegFile, RootSlot, AARCH64_DWARF_REG_SP};

struct FakeRegs {
  sp: u64,
}

impl RegFile for FakeRegs {
  fn get(&self, dwarf_reg: u16) -> Option<u64> {
    match dwarf_reg {
      AARCH64_DWARF_REG_SP => Some(self.sp),
      _ => None,
    }
  }
}

fn align_up(v: usize, align: usize) -> usize {
  (v + (align - 1)) & !(align - 1)
}

#[test]
fn aarch64_sp_reconstruction_and_dwarf_sp_mapping() {
  // Build a synthetic "frame" in a byte buffer and choose an FP value that
  // matches the AArch64 frame-pointer prologue convention.
  //
  // We pick `sp_at_call` and derive `fp` such that:
  //   sp_at_call = fp + 16 - stack_size
  //
  // (see `compute_sp_aarch64` for details).
  let stack_size: u64 = 64;

  let mut stack = vec![0u8; 512];
  let base = stack.as_mut_ptr() as usize;

  let sp_at_call = align_up(base + 128, 16);
  let fp = sp_at_call + (stack_size as usize) - 16;

  // Minimal frame record layout at [FP]:
  //   [FP + 0] = saved FP
  //   [FP + 8] = saved LR
  //
  // (not used directly by this test, but matches the documented AArch64 layout).
  unsafe {
    (fp as *mut u64).write_unaligned(0);
    ((fp + 8) as *mut u64).write_unaligned(0);
  }

  let derived_sp = compute_sp_aarch64(fp, stack_size);
  assert_eq!(derived_sp, sp_at_call, "computed SP must match expected callsite SP");

  // Two synthetic stackmap locations: Indirect [SP + 0] and Indirect [SP + 8].
  let loc0 = Location::Indirect {
    size: 8,
    dwarf_reg: AARCH64_DWARF_REG_SP,
    offset: 0,
  };
  let loc1 = Location::Indirect {
    size: 8,
    dwarf_reg: AARCH64_DWARF_REG_SP,
    offset: 8,
  };

  let regs = FakeRegs {
    sp: derived_sp as u64,
  };

  let RootSlot::StackAddr(addr0) = eval_location(&loc0, &regs).unwrap() else {
    panic!("expected stack slot for loc0");
  };
  let RootSlot::StackAddr(addr1) = eval_location(&loc1, &regs).unwrap() else {
    panic!("expected stack slot for loc1");
  };

  assert_eq!(addr0 as usize, sp_at_call);
  assert_eq!(addr1 as usize, sp_at_call + 8);

  // Validate DWARF register slot mapping for AArch64 RegContext (DWARF 31 => SP).
  let mut ctx = RegContext::default();
  ctx.sp = 0x1111_2222_3333_4444;

  unsafe {
    let sp_slot = ctx.reg_slot_ptr(31).expect("DWARF 31 (SP) should be supported");
    *sp_slot = 0xaaaa_bbbb_cccc_dddd;
  }
  assert_eq!(ctx.sp, 0xaaaa_bbbb_cccc_dddd);
}
