use crate::GcString;

/// A heap-allocated JS Symbol record.
///
/// Note: equality for `GcSymbol` is based on handle identity. `id` exists for
/// debug/introspection and is monotonically assigned by the heap.
#[derive(Debug)]
pub struct JsSymbol {
  id: u64,
  description: Option<GcString>,
  internal: bool,
}

impl JsSymbol {
  pub(crate) fn new(id: u64, description: Option<GcString>, internal: bool) -> Self {
    Self {
      id,
      description,
      internal,
    }
  }

  pub fn id(&self) -> u64 {
    self.id
  }

  /// Returns `true` if this symbol is engine-internal and must not be observable from JavaScript.
  ///
  /// This is used for internal-slot markers and private-name keys, which are stored as
  /// symbol-keyed properties but must be filtered out by `[[OwnPropertyKeys]]` so they are not
  /// returned by `Reflect.ownKeys`, `Object.getOwnPropertySymbols`, etc.
  pub fn is_internal(&self) -> bool {
    self.internal
  }

  pub fn description(&self) -> Option<GcString> {
    self.description
  }
}
