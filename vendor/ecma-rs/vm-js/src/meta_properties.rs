#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(transparent)]
pub(crate) struct MetaPropertyContext(u8);

impl MetaPropertyContext {
  const ALLOW_NEW_TARGET: u8 = 1 << 0;
  const ALLOW_SUPER_PROPERTY: u8 = 1 << 1;
  const ALLOW_SUPER_CALL: u8 = 1 << 2;

  pub(crate) const SCRIPT: Self = Self(0);
  pub(crate) const FUNCTION: Self = Self(Self::ALLOW_NEW_TARGET);
  pub(crate) const METHOD: Self = Self(Self::ALLOW_NEW_TARGET | Self::ALLOW_SUPER_PROPERTY);
  pub(crate) const DERIVED_CONSTRUCTOR: Self =
    Self(Self::ALLOW_NEW_TARGET | Self::ALLOW_SUPER_PROPERTY | Self::ALLOW_SUPER_CALL);

  /// Permissive context used for reparsing nested snippets where the enclosing meta-property
  /// context is unknown (but was already validated by the original full-source parse).
  pub(crate) const ALL: Self =
    Self(Self::ALLOW_NEW_TARGET | Self::ALLOW_SUPER_PROPERTY | Self::ALLOW_SUPER_CALL);

  pub(crate) const fn allow_new_target(self) -> bool {
    self.0 & Self::ALLOW_NEW_TARGET != 0
  }

  pub(crate) const fn allow_super_property(self) -> bool {
    self.0 & Self::ALLOW_SUPER_PROPERTY != 0
  }

  pub(crate) const fn allow_super_call(self) -> bool {
    self.0 & Self::ALLOW_SUPER_CALL != 0
  }

  /// Meta-property context for an arrow function created in `enclosing`.
  ///
  /// Arrow functions do **not** introduce new `new.target`/`super` bindings; they inherit them
  /// lexically from their enclosing context.
  pub(crate) const fn for_arrow(enclosing: Self) -> Self {
    enclosing
  }
}
