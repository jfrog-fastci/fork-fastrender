#![forbid(unsafe_code)]

mod default_value;
mod eval;
mod parser;
mod types;
mod value;

pub use default_value::{DefaultValue, NumericLiteral};
pub use eval::eval_default_value;
pub use parser::{parse_default_value, parse_idl_type, parse_idl_type_complete, ParseError};
pub use types::{
  DistinguishabilityCategory, IdlType, NamedType, NamedTypeKind, NumericType, StringType,
  TypeAnnotation,
};
pub use value::{
  DictionaryMemberSchema, DictionarySchema, PlatformObject, TypeContext, WebIdlException,
  WebIdlValue,
};
