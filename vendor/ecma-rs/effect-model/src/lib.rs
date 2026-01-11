use bitflags::bitflags;

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum ThrowBehavior {
  #[cfg_attr(feature = "serde", serde(alias = "Never"))]
  Never,
  #[cfg_attr(feature = "serde", serde(alias = "Maybe"))]
  Maybe,
  #[cfg_attr(feature = "serde", serde(alias = "Always"))]
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
/// This intentionally ignores *throwing*: whether an operation may throw is
/// tracked separately via [`EffectSet::MAY_THROW`].
///
/// The taxonomy is small by design and can be joined conservatively.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum Purity {
  /// No observable effects.
  #[cfg_attr(feature = "serde", serde(alias = "Pure"))]
  Pure,
  /// May read observable state, but performs no writes/allocations/IO.
  #[cfg_attr(feature = "serde", serde(rename = "readonly", alias = "ReadOnly", alias = "read_only"))]
  ReadOnly,
  /// Allocates, but performs no writes/IO.
  #[cfg_attr(feature = "serde", serde(alias = "Allocating"))]
  Allocating,
  /// Performs some observable effect (writes/IO/etc), or could not be analyzed.
  #[cfg_attr(feature = "serde", serde(alias = "Impure"))]
  Impure,
}

impl Purity {
  #[inline]
  pub const fn join(a: Self, b: Self) -> Self {
    use Purity::*;
    match (a, b) {
      (Impure, _) | (_, Impure) => Impure,
      (Allocating, _) | (_, Allocating) => Allocating,
      (ReadOnly, _) | (_, ReadOnly) => ReadOnly,
      (Pure, Pure) => Pure,
    }
  }
}

bitflags! {
  #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
  pub struct EffectSet: u32 {
    const ALLOCATES = 1 << 0;
    const IO = 1 << 1;
    const NETWORK = 1 << 2;
    const NONDETERMINISTIC = 1 << 3;
    const READS_GLOBAL = 1 << 4;
    const WRITES_GLOBAL = 1 << 5;
    const MAY_THROW = 1 << 6;
    const UNKNOWN = 1 << 7;
  }
}

/// Backwards-compatible alias (older code used `EffectFlags`).
pub type EffectFlags = EffectSet;

impl EffectSet {
  /// Conservative approximation for "unknown effects".
  ///
  /// This is commonly used for:
  /// - unresolved calls
  /// - spread arguments
  /// - any construct we don't currently model precisely
  ///
  /// We set [`EffectSet::UNKNOWN`] to indicate "could do anything" and
  /// [`EffectSet::MAY_THROW`] because unknown operations may raise exceptions.
  pub const UNKNOWN_CALL: Self =
    Self::from_bits_truncate(Self::UNKNOWN.bits() | Self::MAY_THROW.bits());

  /// Convert this bitset into an [`EffectSummary`].
  ///
  /// Note: this conversion is lossy; [`EffectSet`] only encodes a boolean
  /// `MAY_THROW` flag, so the resulting [`ThrowBehavior`] is at most `Maybe`.
  pub fn to_effect_summary(self) -> EffectSummary {
    EffectSummary {
      flags: self & !EffectSet::MAY_THROW,
      throws: if self.contains(EffectSet::MAY_THROW) {
        ThrowBehavior::Maybe
      } else {
        ThrowBehavior::Never
      },
    }
  }

  /// Infer a coarse purity classification from effect flags.
  ///
  /// This is intentionally conservative and ignores `MAY_THROW`, which is
  /// tracked separately from purity in this model.
  pub fn inferred_purity(self) -> Purity {
    let flags = EffectSet::from_bits_truncate(self.bits() & !EffectSet::MAY_THROW.bits());
    if flags.is_empty() {
      return Purity::Pure;
    }
    if flags.contains(EffectSet::UNKNOWN) || flags.contains(EffectSet::WRITES_GLOBAL) {
      return Purity::Impure;
    }
    if flags.contains(EffectSet::IO) || flags.contains(EffectSet::NETWORK) {
      return Purity::Impure;
    }
    if flags.contains(EffectSet::ALLOCATES) {
      return Purity::Allocating;
    }
    if flags.contains(EffectSet::READS_GLOBAL) || flags.contains(EffectSet::NONDETERMINISTIC) {
      return Purity::ReadOnly;
    }
    Purity::Impure
  }
}

#[cfg(feature = "serde")]
mod effect_set_serde {
  use super::{EffectSet, ThrowBehavior};
  use core::fmt;
  use serde::de::{MapAccess, SeqAccess, Visitor};
  use serde::{Deserialize, Deserializer, Serialize, Serializer};

  fn normalize_token(raw: &str) -> String {
    raw
      .trim()
      .to_ascii_uppercase()
      .replace([' ', '-'], "_")
  }

  fn parse_token<E: serde::de::Error>(raw: &str) -> Result<EffectSet, E> {
    match normalize_token(raw).as_str() {
      "" | "0" | "EMPTY" | "NONE" => Ok(EffectSet::empty()),
      "ALLOCATES" => Ok(EffectSet::ALLOCATES),
      "MAY_THROW" | "MAYTHROW" => Ok(EffectSet::MAY_THROW),
      "IO" => Ok(EffectSet::IO),
      "NETWORK" => Ok(EffectSet::NETWORK),
      "NONDETERMINISTIC" | "NON_DETERMINISTIC" => Ok(EffectSet::NONDETERMINISTIC),
      "READS_GLOBAL" | "READ_GLOBAL" => Ok(EffectSet::READS_GLOBAL),
      "WRITES_GLOBAL" | "WRITE_GLOBAL" => Ok(EffectSet::WRITES_GLOBAL),
      "UNKNOWN" => Ok(EffectSet::UNKNOWN),
      other => Err(E::custom(format!("unknown effect flag `{other}`"))),
    }
  }

  fn parse_expr<E: serde::de::Error>(raw: &str) -> Result<EffectSet, E> {
    let raw = raw.trim();
    if raw.is_empty() {
      return Ok(EffectSet::empty());
    }

    let mut out = EffectSet::empty();
    for part in raw.split('|') {
      let part = part.trim();
      if part.is_empty() {
        continue;
      }
      out |= parse_token::<E>(part)?;
    }
    Ok(out)
  }

  impl Serialize for EffectSet {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
      if self.is_empty() {
        return serializer.serialize_str("0");
      }

      // Stable ordering: sort by bit position.
      let mut parts = Vec::new();
      let ordered = [
        (EffectSet::ALLOCATES, "ALLOCATES"),
        (EffectSet::IO, "IO"),
        (EffectSet::NETWORK, "NETWORK"),
        (EffectSet::NONDETERMINISTIC, "NONDETERMINISTIC"),
        (EffectSet::READS_GLOBAL, "READS_GLOBAL"),
        (EffectSet::WRITES_GLOBAL, "WRITES_GLOBAL"),
        (EffectSet::MAY_THROW, "MAY_THROW"),
        (EffectSet::UNKNOWN, "UNKNOWN"),
      ];
      for (flag, name) in ordered {
        if self.contains(flag) {
          parts.push(name);
        }
      }
      serializer.serialize_str(&parts.join(" | "))
    }
  }

  impl<'de> Deserialize<'de> for EffectSet {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
      struct EffectSetVisitor;

      impl<'de> Visitor<'de> for EffectSetVisitor {
        type Value = EffectSet;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
          f.write_str("an effect flag expression (e.g. `IO | NETWORK`), a list of flags, or `{flags, throws}`")
        }

        fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<Self::Value, E> {
          parse_expr(v)
        }

        fn visit_string<E: serde::de::Error>(self, v: String) -> Result<Self::Value, E> {
          self.visit_str(&v)
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Self::Value, A::Error> {
          let mut out = EffectSet::empty();
          while let Some(item) = seq.next_element::<String>()? {
            out |= parse_token::<A::Error>(&item)?;
          }
          Ok(out)
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
          let mut flags: Option<EffectSet> = None;
          let mut throws: Option<ThrowBehavior> = None;

          while let Some(key) = map.next_key::<String>()? {
            match normalize_token(&key).as_str() {
              "FLAGS" => {
                flags = Some(map.next_value::<EffectSet>()?);
              }
              "THROWS" => {
                throws = Some(map.next_value::<ThrowBehavior>()?);
              }
              _ => {
                let _: serde::de::IgnoredAny = map.next_value()?;
              }
            }
          }

          let mut out = flags.unwrap_or_default();
          if let Some(throws) = throws {
            if !matches!(throws, ThrowBehavior::Never) {
              out |= EffectSet::MAY_THROW;
            }
          }
          Ok(out)
        }

        fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<Self::Value, E> {
          Ok(EffectSet::from_bits_truncate(v as u32))
        }

        fn visit_i64<E: serde::de::Error>(self, v: i64) -> Result<Self::Value, E> {
          Ok(EffectSet::from_bits_truncate(v as u32))
        }
      }

      deserializer.deserialize_any(EffectSetVisitor)
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct EffectSummary {
  pub flags: EffectFlags,
  pub throws: ThrowBehavior,
}

#[cfg(feature = "serde")]
mod effect_summary_serde {
  use super::{EffectFlags, EffectSet, EffectSummary, ThrowBehavior};
  use serde::ser::SerializeStruct;
  use serde::{Deserialize, Deserializer, Serialize, Serializer};

  impl Serialize for EffectSummary {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
      let has_may_throw = self.flags.contains(EffectSet::MAY_THROW);
      let flags = self.flags & !EffectSet::MAY_THROW;
      let throws = if has_may_throw {
        ThrowBehavior::join(self.throws, ThrowBehavior::Maybe)
      } else {
        self.throws
      };

      let mut out = serializer.serialize_struct("EffectSummary", 2)?;
      out.serialize_field("flags", &flags)?;
      out.serialize_field("throws", &throws)?;
      out.end()
    }
  }

  impl<'de> Deserialize<'de> for EffectSummary {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
      #[derive(Deserialize)]
      #[serde(untagged)]
      enum Repr {
        Summary { flags: EffectFlags, throws: ThrowBehavior },
        Flags(EffectSet),
      }

      let summary = match Repr::deserialize(deserializer)? {
        Repr::Summary { flags, throws } => {
          let has_may_throw = flags.contains(EffectSet::MAY_THROW);
          let flags = flags & !EffectSet::MAY_THROW;
          let throws = if has_may_throw {
            ThrowBehavior::join(throws, ThrowBehavior::Maybe)
          } else {
            throws
          };
          EffectSummary { flags, throws }
        }
        Repr::Flags(flags) => flags.to_effect_summary(),
      };

      Ok(summary)
    }
  }
}

impl EffectSummary {
  #[inline]
  pub const fn is_pure(&self) -> bool {
    let has_may_throw = (self.flags.bits() & EffectSet::MAY_THROW.bits()) != 0;
    let flags = EffectFlags::from_bits_truncate(self.flags.bits() & !EffectSet::MAY_THROW.bits());
    let throws = if has_may_throw {
      ThrowBehavior::join(self.throws, ThrowBehavior::Maybe)
    } else {
      self.throws
    };

    flags.is_empty() && matches!(throws, ThrowBehavior::Never)
  }

  #[inline]
  pub const fn join(a: Self, b: Self) -> Self {
    let flags_bits = a.flags.bits() | b.flags.bits();
    let has_may_throw = (flags_bits & EffectSet::MAY_THROW.bits()) != 0;
    Self {
      flags: EffectFlags::from_bits_truncate(flags_bits & !EffectSet::MAY_THROW.bits()),
      throws: if has_may_throw {
        ThrowBehavior::join(ThrowBehavior::join(a.throws, b.throws), ThrowBehavior::Maybe)
      } else {
        ThrowBehavior::join(a.throws, b.throws)
      },
    }
  }

  #[inline]
  pub const fn inferred_purity(&self) -> Purity {
    // Note: this is intentionally coarse. Throw behavior is tracked separately
    // (see `EffectSet::MAY_THROW` in the API semantics layer), so we do not
    // force `Impure` purely because something may throw.
    let flags = EffectFlags::from_bits_truncate(self.flags.bits() & !EffectSet::MAY_THROW.bits());
    if flags.is_empty() {
      return Purity::Pure;
    }
    if flags.contains(EffectFlags::UNKNOWN) || flags.contains(EffectFlags::WRITES_GLOBAL) {
      return Purity::Impure;
    }
    if flags.contains(EffectFlags::IO) || flags.contains(EffectFlags::NETWORK) {
      return Purity::Impure;
    }
    if flags.contains(EffectFlags::ALLOCATES) {
      return Purity::Allocating;
    }
    if flags.contains(EffectFlags::READS_GLOBAL) || flags.contains(EffectFlags::NONDETERMINISTIC) {
      return Purity::ReadOnly;
    }
    Purity::Impure
  }

  /// Convert this summary into an [`EffectSet`].
  ///
  /// Note: this conversion is lossy; [`ThrowBehavior::Always`] and
  /// [`ThrowBehavior::Maybe`] are both mapped to [`EffectSet::MAY_THROW`].
  pub fn to_effect_set(self) -> EffectSet {
    let has_may_throw = self.flags.contains(EffectSet::MAY_THROW);
    let mut out = self.flags & !EffectSet::MAY_THROW;
    if has_may_throw || !matches!(self.throws, ThrowBehavior::Never) {
      out |= EffectSet::MAY_THROW;
    }
    out
  }
}

impl EffectSummary {
  pub const PURE: Self = Self {
    flags: EffectFlags::empty(),
    throws: ThrowBehavior::Never,
  };

  /// Conservative approximation for an "unknown call" at the summary layer.
  ///
  /// This corresponds to [`EffectSet::UNKNOWN_CALL`] converted into an
  /// [`EffectSummary`]:
  /// - sets the `UNKNOWN` flag
  /// - sets `throws = Maybe`
  ///
  /// Note that we intentionally keep `MAY_THROW` out of `flags`; throwing is
  /// tracked separately via [`ThrowBehavior`].
  pub const UNKNOWN_CALL: Self = Self {
    flags: EffectFlags::UNKNOWN,
    throws: ThrowBehavior::Maybe,
  };

  pub const UNKNOWN: Self = Self {
    flags: EffectFlags::from_bits_truncate(
      EffectFlags::all().bits() & !EffectSet::MAY_THROW.bits(),
    ),
    throws: ThrowBehavior::Maybe,
  };
}

impl From<EffectSet> for EffectSummary {
  fn from(value: EffectSet) -> Self {
    value.to_effect_summary()
  }
}

impl From<EffectSummary> for EffectSet {
  fn from(value: EffectSummary) -> Self {
    value.to_effect_set()
  }
}

/// An effect template used for API semantics where some effects may be
/// conditional on runtime callback behavior.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum EffectTemplate {
  #[cfg_attr(feature = "serde", serde(alias = "Pure"))]
  Pure,
  #[cfg_attr(feature = "serde", serde(alias = "Io"))]
  Io,
  #[cfg_attr(feature = "serde", serde(alias = "Custom"))]
  Custom(EffectSet),
  #[cfg_attr(feature = "serde", serde(alias = "DependsOnArgs"))]
  DependsOnArgs { base: EffectSet, args: Vec<usize> },
  #[cfg_attr(feature = "serde", serde(alias = "Unknown"))]
  Unknown,
}

impl Default for EffectTemplate {
  fn default() -> Self {
    Self::Unknown
  }
}

impl EffectTemplate {
  /// Base effects for this template, ignoring any callback/argument-dependent behavior.
  ///
  /// This is mainly used when building/validating API databases, where we need a
  /// conservative non-template summary (e.g. to sanity-check purity metadata).
  pub fn base_effects(&self) -> EffectSet {
    match self {
      Self::Pure => EffectSet::empty(),
      Self::Io => EffectSet::IO | EffectSet::MAY_THROW,
      Self::Custom(base) => *base,
      Self::DependsOnArgs { base, .. } => *base,
      Self::Unknown => EffectSet::UNKNOWN_CALL,
    }
  }

  pub fn apply(&self, arg_effects: &[EffectSet]) -> EffectSet {
    match self {
      Self::Pure => EffectSet::empty(),
      Self::Io => EffectSet::IO | EffectSet::MAY_THROW,
      Self::Custom(base) => *base,
      Self::DependsOnArgs { base, args } => {
        let mut effects = *base;
        for &idx in args {
          if let Some(arg) = arg_effects.get(idx) {
            effects |= *arg;
          } else {
            debug_assert!(
              idx < arg_effects.len(),
              "EffectTemplate::apply arg index {idx} out of range (len={})",
              arg_effects.len()
            );
            effects |= EffectSet::UNKNOWN_CALL;
          }
        }
        effects
      }
      Self::Unknown => EffectSet::UNKNOWN_CALL,
    }
  }
}

/// A purity template used for API semantics where purity may be conditional on
/// runtime callback behavior.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum PurityTemplate {
  #[cfg_attr(feature = "serde", serde(alias = "Pure"))]
  Pure,
  #[cfg_attr(feature = "serde", serde(rename = "readonly", alias = "ReadOnly", alias = "read_only"))]
  ReadOnly,
  #[cfg_attr(feature = "serde", serde(alias = "Allocating"))]
  Allocating,
  #[cfg_attr(feature = "serde", serde(alias = "Impure"))]
  Impure,
  #[cfg_attr(feature = "serde", serde(alias = "DependsOnArgs"))]
  DependsOnArgs { base: Purity, args: Vec<usize> },
  #[cfg_attr(feature = "serde", serde(alias = "Unknown"))]
  Unknown,
}

impl Default for PurityTemplate {
  fn default() -> Self {
    Self::Unknown
  }
}

impl PurityTemplate {
  /// Base purity for this template, ignoring any callback/argument-dependent behavior.
  pub fn base_purity(&self) -> Purity {
    match self {
      Self::Pure => Purity::Pure,
      Self::ReadOnly => Purity::ReadOnly,
      Self::Allocating => Purity::Allocating,
      Self::Impure => Purity::Impure,
      Self::DependsOnArgs { base, .. } => *base,
      Self::Unknown => Purity::Impure,
    }
  }

  pub fn apply(&self, arg_purity: &[Purity]) -> Purity {
    match self {
      Self::Pure => Purity::Pure,
      Self::ReadOnly => Purity::ReadOnly,
      Self::Allocating => Purity::Allocating,
      Self::Impure => Purity::Impure,
      Self::DependsOnArgs { base, args } => {
        if matches!(base, Purity::Impure) {
          return Purity::Impure;
        }
        let mut saw_readonly = matches!(base, Purity::ReadOnly);
        let mut saw_allocating = matches!(base, Purity::Allocating);
        for &idx in args {
          let Some(p) = arg_purity.get(idx).copied() else {
            debug_assert!(
              idx < arg_purity.len(),
              "PurityTemplate::apply arg index {idx} out of range (len={})",
              arg_purity.len()
            );
            return Purity::Impure;
          };
          match p {
            Purity::Impure => return Purity::Impure,
            Purity::Allocating => saw_allocating = true,
            Purity::ReadOnly => saw_readonly = true,
            Purity::Pure => {}
          }
        }
        if saw_allocating {
          Purity::Allocating
        } else if saw_readonly {
          Purity::ReadOnly
        } else {
          Purity::Pure
        }
      }
      Self::Unknown => Purity::Impure,
    }
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
    // Throwing does not automatically imply impurity at this coarse layer; we
    // track throws separately via `EffectSet::MAY_THROW`.
    assert_eq!(throwing.inferred_purity(), Purity::Pure);

    // Backwards compatibility: older data sometimes encoded MAY_THROW in flags.
    // Purity inference should ignore this bit (throwing is tracked separately).
    let throwing_flag = EffectSummary {
      flags: EffectFlags::MAY_THROW,
      throws: ThrowBehavior::Never,
    };
    assert_eq!(throwing_flag.inferred_purity(), Purity::Pure);
  }

  #[test]
  fn unknown_template_is_conservative_about_throwing() {
    let effects = EffectTemplate::Unknown.apply(&[]);
    assert!(effects.contains(EffectSet::UNKNOWN));
    assert!(effects.contains(EffectSet::MAY_THROW));
  }

  #[test]
  fn base_methods_ignore_arg_deps() {
    let effects = EffectTemplate::DependsOnArgs {
      base: EffectSet::ALLOCATES,
      args: vec![0],
    };
    // `apply(&[])` would mark this as unknown due to out-of-range args, but
    // `base_effects()` should not.
    assert_eq!(effects.base_effects(), EffectSet::ALLOCATES);

    let purity = PurityTemplate::DependsOnArgs {
      base: Purity::ReadOnly,
      args: vec![0],
    };
    assert_eq!(purity.base_purity(), Purity::ReadOnly);

    assert_eq!(
      EffectSet::UNKNOWN_CALL,
      EffectSet::UNKNOWN | EffectSet::MAY_THROW
    );

    assert_eq!(
      EffectSet::UNKNOWN_CALL.to_effect_summary().to_effect_set(),
      EffectSet::UNKNOWN_CALL
    );

    assert_eq!(
      EffectSummary::UNKNOWN_CALL.to_effect_set(),
      EffectSet::UNKNOWN_CALL
    );

    assert_eq!(
      EffectSet::UNKNOWN_CALL.to_effect_summary(),
      EffectSummary::UNKNOWN_CALL
    );

    assert_eq!(
      EffectSummary::UNKNOWN.to_effect_set(),
      EffectSet::all()
    );

    assert_eq!(
      EffectSet::all().to_effect_summary(),
      EffectSummary::UNKNOWN
    );
  }

  #[test]
  fn may_throw_in_flags_is_normalized() {
    let summary = EffectSummary {
      flags: EffectFlags::MAY_THROW,
      throws: ThrowBehavior::Never,
    };

    // `is_pure` should treat MAY_THROW as throwing even if it appears in flags.
    assert!(!summary.is_pure());

    // Converting to an EffectSet should preserve the MAY_THROW information.
    assert!(summary.to_effect_set().contains(EffectSet::MAY_THROW));

    // Joining should strip MAY_THROW from flags and carry it into throws.
    let joined = EffectSummary::join(summary, EffectSummary::PURE);
    assert!(!joined.flags.contains(EffectSet::MAY_THROW));
    assert_eq!(joined.throws, ThrowBehavior::Maybe);
  }
}
