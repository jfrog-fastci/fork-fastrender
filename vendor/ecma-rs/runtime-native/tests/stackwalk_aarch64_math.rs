use runtime_native::arch::aarch64::RegContext;
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
fn aarch64_callsite_sp_and_dwarf_sp_mapping() {
  // On AArch64 with frame pointers enabled, each frame record is:
  //   [FP + 0] = saved FP
  //   [FP + 8] = saved LR
  //
  // and the caller's stack pointer at the callsite return address (stackmap `SP`)
  // is:
  //   caller_sp_callsite = callee_fp + 16
  let mut stack = vec![0u8; 512];
  let base = stack.as_mut_ptr() as usize;

  let caller_sp = align_up(base + 128, 16);
  let callee_fp = caller_sp - 16;
  let derived_caller_sp = callee_fp + 16;
  assert_eq!(
    derived_caller_sp, caller_sp,
    "caller SP at the callsite must be callee_fp + 16"
  );

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
    sp: derived_caller_sp as u64,
  };

  let RootSlot::StackAddr(addr0) = eval_location(&loc0, &regs).unwrap() else {
    panic!("expected stack slot for loc0");
  };
  let RootSlot::StackAddr(addr1) = eval_location(&loc1, &regs).unwrap() else {
    panic!("expected stack slot for loc1");
  };

  assert_eq!(addr0 as usize, caller_sp);
  assert_eq!(addr1 as usize, caller_sp + 8);

  // Validate DWARF register slot mapping for AArch64 RegContext (DWARF 31 => SP).
  let mut ctx = RegContext::default();
  ctx.sp = 0x1111_2222_3333_4444;

  unsafe {
    let sp_slot = ctx.reg_slot_ptr(31).expect("DWARF 31 (SP) should be supported");
    *sp_slot = 0xaaaa_bbbb_cccc_dddd;
  }
  assert_eq!(ctx.sp, 0xaaaa_bbbb_cccc_dddd);
}
