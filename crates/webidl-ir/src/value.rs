use crate::{DefaultValue, IdlType};
use std::any::Any;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

/// Opaque host value carried through WebIDL conversion layers.
///
/// This is primarily used as a placeholder for interface/object return values: bindings can embed a
/// runtime-specific handle (e.g. a JS object reference) inside a [`WebIdlValue`] and let the
/// runtime convert it back into an ECMAScript value.
#[derive(Clone)]
pub struct PlatformObject(Rc<dyn Any>);

impl PlatformObject {
  pub fn new<T: Any>(value: T) -> Self {
    Self(Rc::new(value))
  }

  pub fn downcast_ref<T: Any>(&self) -> Option<&T> {
    self.0.as_ref().downcast_ref::<T>()
  }
}

impl std::fmt::Debug for PlatformObject {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    f.write_str("PlatformObject(..)")
  }
}

impl PartialEq for PlatformObject {
  fn eq(&self, other: &Self) -> bool {
    Rc::ptr_eq(&self.0, &other.0)
  }
}

#[derive(Debug, Clone, PartialEq)]
pub enum WebIdlValue {
  Undefined,
  Null,
  Boolean(bool),

  Byte(i8),
  Octet(u8),
  Short(i16),
  UnsignedShort(u16),
  Long(i32),
  UnsignedLong(u32),
  LongLong(i64),
  UnsignedLongLong(u64),
  Float(f32),
  UnrestrictedFloat(f32),
  Double(f64),
  UnrestrictedDouble(f64),

  String(String),
  Enum(String),

  Sequence {
    elem_ty: Box<IdlType>,
    values: Vec<WebIdlValue>,
  },
  Record {
    key_ty: Box<IdlType>,
    value_ty: Box<IdlType>,
    entries: BTreeMap<String, WebIdlValue>,
  },
  Dictionary {
    name: String,
    members: BTreeMap<String, WebIdlValue>,
  },
  Union {
    member_ty: Box<IdlType>,
    value: Box<WebIdlValue>,
  },
  /// Opaque platform value (typically a runtime-owned JS object handle).
  PlatformObject(PlatformObject),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WebIdlException {
  TypeError { message: String },
  RangeError { message: String },
}

impl WebIdlException {
  pub fn type_error(message: impl Into<String>) -> Self {
    WebIdlException::TypeError {
      message: message.into(),
    }
  }

  pub fn range_error(message: impl Into<String>) -> Self {
    WebIdlException::RangeError {
      message: message.into(),
    }
  }
}

impl std::fmt::Display for WebIdlException {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      WebIdlException::TypeError { message } => write!(f, "TypeError: {message}"),
      WebIdlException::RangeError { message } => write!(f, "RangeError: {message}"),
    }
  }
}

impl std::error::Error for WebIdlException {}

#[derive(Debug, Clone, PartialEq)]
pub struct DictionaryMemberSchema {
  pub name: String,
  pub required: bool,
  pub ty: IdlType,
  pub default: Option<DefaultValue>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DictionarySchema {
  pub name: String,
  pub inherits: Option<String>,
  pub members: Vec<DictionaryMemberSchema>,
}

#[derive(Debug, Default, Clone)]
pub struct TypeContext {
  pub enums: BTreeMap<String, BTreeSet<String>>,
  pub dictionaries: BTreeMap<String, DictionarySchema>,
  pub typedefs: BTreeMap<String, IdlType>,
}

impl TypeContext {
  pub fn add_enum(
    &mut self,
    name: impl Into<String>,
    values: impl IntoIterator<Item = impl Into<String>>,
  ) {
    self.enums.insert(
      name.into(),
      values.into_iter().map(Into::into).collect::<BTreeSet<_>>(),
    );
  }

  pub fn add_dictionary(&mut self, dict: DictionarySchema) {
    self.dictionaries.insert(dict.name.clone(), dict);
  }

  pub fn add_typedef(&mut self, name: impl Into<String>, ty: IdlType) {
    self.typedefs.insert(name.into(), ty);
  }

  pub fn flattened_dictionary_members(&self, name: &str) -> Option<Vec<DictionaryMemberSchema>> {
    let mut out = Vec::<DictionaryMemberSchema>::new();
    let mut visited = BTreeSet::<String>::new();
    self.flattened_dictionary_members_inner(name, &mut visited, &mut out);
    if out.is_empty() && !self.dictionaries.contains_key(name) {
      return None;
    }
    Some(out)
  }

  fn flattened_dictionary_members_inner(
    &self,
    name: &str,
    visited: &mut BTreeSet<String>,
    out: &mut Vec<DictionaryMemberSchema>,
  ) {
    if !visited.insert(name.to_string()) {
      return;
    }
    let Some(dict) = self.dictionaries.get(name) else {
      return;
    };

    if let Some(parent) = &dict.inherits {
      self.flattened_dictionary_members_inner(parent, visited, out);
    }

    // WebIDL dictionary algorithms iterate members in lexicographical order within each dictionary:
    // <https://webidl.spec.whatwg.org/#js-to-dictionary>
    // <https://webidl.spec.whatwg.org/#dictionary-to-js>
    let mut members = dict.members.clone();
    members.sort_by(|a, b| a.name.cmp(&b.name));
    out.extend(members);
  }
}
