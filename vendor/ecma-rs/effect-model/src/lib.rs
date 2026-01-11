use bitflags::bitflags;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum ThrowBehavior {
  Never,
  Maybe,
  Always,
}

impl ThrowBehavior {
  #[inline]
  pub const fn join(a: Self, b: Self) -> Self {
    use ThrowBehavior::*;
    match (a, b) {
      (Always, _) | (_, Always) => Always,
      (Maybe, _) | (_, Maybe) => Maybe,
      (Never, Never) => Never,
    }
  }
}

impl Default for ThrowBehavior {
  fn default() -> Self {
    Self::Never
  }
}

/// A coarse purity classification useful for both API semantics and program
/// analyses.
///
/// This is intentionally a small taxonomy; it can be joined conservatively.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum Purity {
  /// No observable effects (including no throws).
  Pure,
  /// May read observable state, but performs no writes/allocations/IO.
  ReadOnly,
  /// Only allocates (and does not throw).
  Allocating,
  /// Performs some observable effect (writes/IO/throws/etc).
  Impure,
  /// Unknown / could not be analyzed.
  Unknown,
}

impl Purity {
  #[inline]
  pub const fn join(a: Self, b: Self) -> Self {
    use Purity::*;
    match (a, b) {
      (Unknown, _) | (_, Unknown) => Unknown,
      (Impure, _) | (_, Impure) => Impure,
      (Allocating, ReadOnly) | (ReadOnly, Allocating) => Impure,
      (Allocating, _) | (_, Allocating) => Allocating,
      (ReadOnly, _) | (_, ReadOnly) => ReadOnly,
      (Pure, Pure) => Pure,
    }
  }
}

bitflags! {
  #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
  #[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
  pub struct EffectFlags: u32 {
    const ALLOCATES = 1 << 0;
    const IO = 1 << 1;
    const NETWORK = 1 << 2;
    const NONDETERMINISTIC = 1 << 3;
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct EffectSummary {
  pub flags: EffectFlags,
  pub throws: ThrowBehavior,
}

impl EffectSummary {
  #[inline]
  pub const fn is_pure(&self) -> bool {
    self.flags.is_empty() && matches!(self.throws, ThrowBehavior::Never)
  }

  #[inline]
  pub const fn join(a: Self, b: Self) -> Self {
    Self {
      flags: a.flags.union(b.flags),
      throws: ThrowBehavior::join(a.throws, b.throws),
    }
  }

  #[inline]
  pub const fn inferred_purity(&self) -> Purity {
    if self.is_pure() {
      return Purity::Pure;
    }
    if !matches!(self.throws, ThrowBehavior::Never) {
      return Purity::Impure;
    }
    if self.flags.bits() == EffectFlags::ALLOCATES.bits() {
      return Purity::Allocating;
    }
    Purity::Impure
  }
}

impl EffectSummary {
  pub const PURE: Self = Self {
    flags: EffectFlags::empty(),
    throws: ThrowBehavior::Never,
  };
}

/// An effect template used for API semantics where some effects may be
/// conditional on runtime callback behavior.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum EffectTemplate {
  Pure,
  Io,
  DependsOnCallback,
  Custom(EffectSummary),
  Unknown,
}

impl Default for EffectTemplate {
  fn default() -> Self {
    Self::Unknown
  }
}

/// A purity template used for API semantics where purity may be conditional on
/// runtime callback behavior.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub enum PurityTemplate {
  Pure,
  ReadOnly,
  DependsOnCallback,
  Impure,
  Unknown,
}

impl Default for PurityTemplate {
  fn default() -> Self {
    Self::Unknown
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn join_unions_flags_and_maxes_throws() {
    let a = EffectSummary {
      flags: EffectFlags::ALLOCATES,
      throws: ThrowBehavior::Never,
    };
    let b = EffectSummary {
      flags: EffectFlags::IO | EffectFlags::NETWORK,
      throws: ThrowBehavior::Maybe,
    };

    let joined = EffectSummary::join(a, b);
    assert_eq!(
      joined.flags,
      EffectFlags::ALLOCATES | EffectFlags::IO | EffectFlags::NETWORK
    );
    assert_eq!(joined.throws, ThrowBehavior::Maybe);
  }

  #[test]
  fn inferred_purity() {
    assert_eq!(EffectSummary::PURE.inferred_purity(), Purity::Pure);

    let alloc = EffectSummary {
      flags: EffectFlags::ALLOCATES,
      throws: ThrowBehavior::Never,
    };
    assert_eq!(alloc.inferred_purity(), Purity::Allocating);

    let throwing = EffectSummary {
      flags: EffectFlags::empty(),
      throws: ThrowBehavior::Maybe,
    };
    assert_eq!(throwing.inferred_purity(), Purity::Impure);
  }
}

