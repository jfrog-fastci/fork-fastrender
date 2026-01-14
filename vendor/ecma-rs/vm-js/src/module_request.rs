//! ECMA-262 module request record types.
//!
//! Module loading host hooks and module records (e.g. `[[RequestedModules]]`, `[[LoadedModules]]`)
//! are defined in terms of `ModuleRequest` / `LoadedModuleRequest` records.
//!
//! ## Spec references
//!
//! - `ModuleRequest` record: <https://tc39.es/ecma262/#sec-modulerequest-record>
//! - `ModuleRequestsEqual`: <https://tc39.es/ecma262/#sec-modulerequestsequal>

use std::cmp::Ordering;

use crate::{JsString, VmError};

/// Compare two strings by lexicographic order of UTF-16 code units (ECMA-262 string ordering).
///
/// This intentionally does **not** use Rust's default `str` ordering (UTF-8 byte order).
pub fn cmp_utf16(a: &str, b: &str) -> Ordering {
  let mut a_units = a.encode_utf16();
  let mut b_units = b.encode_utf16();

  loop {
    match (a_units.next(), b_units.next()) {
      (Some(a_u), Some(b_u)) => match a_u.cmp(&b_u) {
        Ordering::Equal => {}
        non_eq => return non_eq,
      },
      (None, Some(_)) => return Ordering::Less,
      (Some(_), None) => return Ordering::Greater,
      (None, None) => return Ordering::Equal,
    }
  }
}

/// An `ImportAttribute` record.
///
/// Spec: <https://tc39.es/ecma262/#importattribute-record>
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ImportAttribute {
  pub key: JsString,
  pub value: JsString,
}

impl ImportAttribute {
  #[inline]
  pub fn new(key: JsString, value: JsString) -> Self {
    Self { key, value }
  }

  #[inline]
  pub fn try_new(key: &str, value: &str) -> Result<Self, VmError> {
    Ok(Self {
      key: JsString::from_str(key)?,
      value: JsString::from_str(value)?,
    })
  }
}

fn cmp_import_attribute(a: &ImportAttribute, b: &ImportAttribute) -> Ordering {
  match a.key.cmp(&b.key) {
    Ordering::Equal => a.value.cmp(&b.value),
    non_eq => non_eq,
  }
}

/// A `ModuleRequest` record.
///
/// Spec: <https://tc39.es/ecma262/#sec-modulerequest-record>
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct ModuleRequest {
  pub specifier: JsString,
  pub attributes: Vec<ImportAttribute>,
}

impl ModuleRequest {
  /// Returns the raw UTF-16 code units of the module specifier.
  #[inline]
  pub fn specifier_code_units(&self) -> &[u16] {
    self.specifier.as_code_units()
  }

  /// Returns a UTF-8 string representation of the module specifier, replacing any unpaired
  /// surrogates with U+FFFD.
  ///
  /// This is intended for debug/error paths and host integrations (e.g. filesystem paths). For
  /// spec-visible equality, use UTF-16 code unit comparisons (via `Eq` / [`module_requests_equal`]).
  #[inline]
  pub fn specifier_utf8_lossy(&self) -> String {
    self.specifier.to_utf8_lossy()
  }

  /// Construct a new module request, canonicalizing the attribute list.
  ///
  /// Canonicalization sorts by `(key, value)` using lexicographic order of UTF-16 code units so:
  /// - the stored representation is deterministic (stable across hosts),
  /// - derived `Eq`/`Hash` become compatible with `ModuleRequestsEqual` when all instances are
  ///   constructed via this constructor (or [`ModuleRequest::canonicalize`]).
  #[inline]
  pub fn new(specifier: JsString, mut attributes: Vec<ImportAttribute>) -> Self {
    // Use an in-place unstable sort to avoid heap allocations. Import attributes are treated as a
    // set by the spec; relative ordering between equal entries is not observable.
    attributes.sort_unstable_by(cmp_import_attribute);
    Self {
      specifier,
      attributes,
    }
  }

  /// Canonicalize this request's attribute list in-place.
  #[inline]
  pub fn canonicalize(&mut self) {
    self.attributes.sort_unstable_by(cmp_import_attribute);
  }

  /// Builder helper: append an import attribute and re-canonicalize.
  #[inline]
  pub fn with_import_attribute(mut self, key: JsString, value: JsString) -> Self {
    self.attributes.push(ImportAttribute::new(key, value));
    self.canonicalize();
    self
  }

  /// Builder helper: append a string import attribute and re-canonicalize.
  #[inline]
  pub fn try_with_import_attribute(mut self, key: &str, value: &str) -> Result<Self, VmError> {
    self.attributes.push(ImportAttribute::try_new(key, value)?);
    self.canonicalize();
    Ok(self)
  }

  /// Implements `ModuleRequestsEqual(left, right)` from ECMA-262.
  ///
  /// Import attributes are compared **order-insensitively** (with a length check).
  ///
  /// Note: when both `ModuleRequest`s are canonicalized (e.g. built via [`ModuleRequest::new`]),
  /// `self == other` is equivalent to this method.
  pub fn spec_equal(&self, other: &Self) -> bool {
    module_requests_equal(self, other)
  }
}

/// A module request record paired with its loaded module record.
///
/// Spec: <https://tc39.es/ecma262/#loadedmodulerequest-record>
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LoadedModuleRequest<M> {
  pub request: ModuleRequest,
  pub module: M,
}

impl<M> LoadedModuleRequest<M> {
  #[inline]
  pub fn new(request: ModuleRequest, module: M) -> Self {
    Self { request, module }
  }
}

/// The subset of fields shared by `ModuleRequest` and `LoadedModuleRequest`.
///
/// This exists so [`module_requests_equal`] can be implemented in the same shape as the spec
/// (`ModuleRequestsEqual` accepts either record).
pub trait ModuleRequestLike {
  fn specifier(&self) -> &JsString;
  fn attributes(&self) -> &[ImportAttribute];
}

impl ModuleRequestLike for ModuleRequest {
  #[inline]
  fn specifier(&self) -> &JsString {
    &self.specifier
  }

  #[inline]
  fn attributes(&self) -> &[ImportAttribute] {
    &self.attributes
  }
}

impl<M> ModuleRequestLike for LoadedModuleRequest<M> {
  #[inline]
  fn specifier(&self) -> &JsString {
    &self.request.specifier
  }

  #[inline]
  fn attributes(&self) -> &[ImportAttribute] {
    &self.request.attributes
  }
}

/// Implements `ModuleRequestsEqual(left, right)` from ECMA-262.
///
/// Spec: <https://tc39.es/ecma262/#sec-modulerequestsequal>
///
/// Import attributes are compared **order-insensitively**.
pub fn module_requests_equal<L: ModuleRequestLike + ?Sized, R: ModuleRequestLike + ?Sized>(
  left: &L,
  right: &R,
) -> bool {
  if left.specifier() != right.specifier() {
    return false;
  }

  let left_attrs = left.attributes();
  let right_attrs = right.attributes();
  if left_attrs.len() != right_attrs.len() {
    return false;
  }

  for l in left_attrs {
    if !right_attrs
      .iter()
      .any(|r| l.key == r.key && l.value == r.value)
    {
      return false;
    }
  }

  true
}
