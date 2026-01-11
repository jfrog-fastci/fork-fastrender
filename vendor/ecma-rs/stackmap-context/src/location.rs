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
      StackMapLocation::Indirect { base_reg, offset } => ctx
        .get_dwarf_reg_u64(base_reg)
        .map(|base| base.wrapping_add(offset as i64 as u64)),
    }
  }
}

#[cfg(test)]
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
