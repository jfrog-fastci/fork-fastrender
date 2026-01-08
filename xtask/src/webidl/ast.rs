//! WebIDL AST nodes used by the WebIDL analysis/codegen pipeline.
//!
//! The top-level Bikeshed WebIDL extractor/parser in [`super`] intentionally stays forgiving and
//! stores interface members as raw strings. This AST is a *second* typed layer used by codegen.

use anyhow::Result;
use std::fmt;
use super::ExtendedAttribute;

/// A WebIDL type expression.
///
/// This is a pragmatic representation for the subset of WebIDL used by the WHATWG specs that we
/// generate bindings from.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum IdlType {
  Builtin(BuiltinType),
  /// A reference to an interface/dictionary/enum/typedef/callback name.
  Named(String),
  Nullable(Box<IdlType>),
  Union(Vec<IdlType>),
  Sequence(Box<IdlType>),
  FrozenArray(Box<IdlType>),
  Promise(Box<IdlType>),
  Record {
    key: Box<IdlType>,
    value: Box<IdlType>,
  },
}

impl IdlType {
  /// Canonicalize this type by expanding any referenced typedefs.
  pub fn canonicalize(&self, world: &super::resolve::ResolvedWebIdlWorld) -> Result<IdlType> {
    self.canonicalize_with(&mut |name| {
      if world.typedefs.contains_key(name) {
        Ok(Some(world.resolve_typedef(name)?))
      } else {
        Ok(None)
      }
    })
  }

  pub(crate) fn canonicalize_with<F>(&self, resolve_named: &mut F) -> Result<IdlType>
  where
    F: FnMut(&str) -> Result<Option<IdlType>>,
  {
    Ok(match self {
      IdlType::Builtin(b) => IdlType::Builtin(*b),
      IdlType::Named(name) => resolve_named(name)?.unwrap_or_else(|| IdlType::Named(name.clone())),
      IdlType::Nullable(inner) => IdlType::Nullable(Box::new(inner.canonicalize_with(resolve_named)?)),
      IdlType::Union(members) => {
        let mut out = Vec::with_capacity(members.len());
        for m in members {
          out.push(m.canonicalize_with(resolve_named)?);
        }
        IdlType::Union(out)
      }
      IdlType::Sequence(inner) => {
        IdlType::Sequence(Box::new(inner.canonicalize_with(resolve_named)?))
      }
      IdlType::FrozenArray(inner) => {
        IdlType::FrozenArray(Box::new(inner.canonicalize_with(resolve_named)?))
      }
      IdlType::Promise(inner) => IdlType::Promise(Box::new(inner.canonicalize_with(resolve_named)?)),
      IdlType::Record { key, value } => IdlType::Record {
        key: Box::new(key.canonicalize_with(resolve_named)?),
        value: Box::new(value.canonicalize_with(resolve_named)?),
      },
    })
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BuiltinType {
  Undefined,
  Any,
  Boolean,
  // Integer types.
  Byte,
  Octet,
  Short,
  UnsignedShort,
  Long,
  UnsignedLong,
  LongLong,
  UnsignedLongLong,
  // Floating types.
  Float,
  UnrestrictedFloat,
  Double,
  UnrestrictedDouble,
  // String types.
  DOMString,
  USVString,
  ByteString,
  Object,
}

impl fmt::Display for BuiltinType {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let s = match self {
      BuiltinType::Undefined => "undefined",
      BuiltinType::Any => "any",
      BuiltinType::Boolean => "boolean",
      BuiltinType::Byte => "byte",
      BuiltinType::Octet => "octet",
      BuiltinType::Short => "short",
      BuiltinType::UnsignedShort => "unsigned short",
      BuiltinType::Long => "long",
      BuiltinType::UnsignedLong => "unsigned long",
      BuiltinType::LongLong => "long long",
      BuiltinType::UnsignedLongLong => "unsigned long long",
      BuiltinType::Float => "float",
      BuiltinType::UnrestrictedFloat => "unrestricted float",
      BuiltinType::Double => "double",
      BuiltinType::UnrestrictedDouble => "unrestricted double",
      BuiltinType::DOMString => "DOMString",
      BuiltinType::USVString => "USVString",
      BuiltinType::ByteString => "ByteString",
      BuiltinType::Object => "object",
    };
    f.write_str(s)
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdlLiteral {
  Null,
  Undefined,
  Boolean(bool),
  /// Numeric literal (kept as source text for deterministic codegen).
  Number(String),
  String(String),
  EmptyObject,
  EmptyArray,
  Identifier(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Argument {
  pub ext_attrs: Vec<ExtendedAttribute>,
  pub name: String,
  pub type_: IdlType,
  pub optional: bool,
  pub variadic: bool,
  pub default: Option<IdlLiteral>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecialOperation {
  Getter,
  Setter,
  Deleter,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InterfaceMember {
  Constructor { arguments: Vec<Argument> },
  Attribute {
    name: String,
    type_: IdlType,
    readonly: bool,
    inherit: bool,
    stringifier: bool,
    static_: bool,
  },
  Operation {
    name: Option<String>,
    return_type: IdlType,
    arguments: Vec<Argument>,
    static_: bool,
    stringifier: bool,
    special: Option<SpecialOperation>,
  },
  Constant {
    name: String,
    type_: IdlType,
    value: IdlLiteral,
  },
  Iterable {
    async_: bool,
    key_type: Option<IdlType>,
    value_type: IdlType,
  },
  Unparsed { raw: String },
}
