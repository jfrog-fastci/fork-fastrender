use crate::{DefaultValue, IdlType};
use std::collections::{BTreeMap, BTreeSet};

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
  Dictionary {
    name: String,
    members: BTreeMap<String, WebIdlValue>,
  },
  Union {
    member_ty: Box<IdlType>,
    value: Box<WebIdlValue>,
  },
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

    out.extend(dict.members.iter().cloned());
  }
}
