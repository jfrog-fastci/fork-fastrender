use crate::ThreadContext;

/// A stackmap location in terms of DWARF register numbers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StackMapLocation {
  /// The value is held directly in a DWARF register.
  Register(u16),

  /// The value is located at `[base_reg + offset]` in memory.
  ///
  /// For LLVM stackmaps, this is typically a stack slot based off SP or FP.
  Indirect { base_reg: u16, offset: i32 },
}

impl StackMapLocation {
  /// Evaluates a stackmap location against a stopped thread's register state.
  ///
  /// - [`StackMapLocation::Register`] returns the register *value*.
  /// - [`StackMapLocation::Indirect`] returns the computed *address* (`base + offset`).
  pub fn evaluate(&self, ctx: &ThreadContext) -> Option<u64> {
    match *self {
      StackMapLocation::Register(regno) => ctx.get_dwarf_reg_u64(regno),
      StackMapLocation::Indirect { base_reg, offset } => {
        let base = ctx.get_dwarf_reg_u64(base_reg)?;
        let addr = (base as i128) + (offset as i128);
        if !(0..=u64::MAX as i128).contains(&addr) {
          return None;
        }
        Some(addr as u64)
      }
    }
  }
}

#[cfg(all(test, any(target_arch = "x86_64", target_arch = "aarch64")))]
mod tests {
  use super::StackMapLocation;
  use crate::{ThreadContext, DWARF_REG_SP};

  #[test]
  fn evaluate_register_and_indirect() {
    let mut ctx = ThreadContext::default();
    ctx.set_dwarf_reg_u64(DWARF_REG_SP, 0x1000).unwrap();

    assert_eq!(
      StackMapLocation::Register(DWARF_REG_SP).evaluate(&ctx),
      Some(0x1000)
    );

    assert_eq!(
      StackMapLocation::Indirect {
        base_reg: DWARF_REG_SP,
        offset: 0x20,
      }
      .evaluate(&ctx),
      Some(0x1020)
    );

    assert_eq!(
      StackMapLocation::Indirect {
        base_reg: DWARF_REG_SP,
        offset: -0x10,
      }
      .evaluate(&ctx),
      Some(0x0ff0)
    );

    ctx.set_dwarf_reg_u64(DWARF_REG_SP, 0).unwrap();
    assert_eq!(
      StackMapLocation::Indirect {
        base_reg: DWARF_REG_SP,
        offset: -1,
      }
      .evaluate(&ctx),
      None
    );

    ctx.set_dwarf_reg_u64(DWARF_REG_SP, u64::MAX).unwrap();
    assert_eq!(
      StackMapLocation::Indirect {
        base_reg: DWARF_REG_SP,
        offset: 1,
      }
      .evaluate(&ctx),
      None
    );

    assert_eq!(StackMapLocation::Register(0xffff).evaluate(&ctx), None);
    assert_eq!(
      StackMapLocation::Indirect {
        base_reg: 0xffff,
        offset: 0,
      }
      .evaluate(&ctx),
      None
    );
  }
}
