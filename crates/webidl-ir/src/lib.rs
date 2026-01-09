#![forbid(unsafe_code)]

mod default_eval;
mod default_value;
mod idl_type;
mod parser;
mod value;

pub use default_eval::eval_default_value;
pub use default_value::{DefaultValue, NumericLiteral};
pub use idl_type::{
  DistinguishabilityCategory, IdlType, NamedType, NamedTypeKind, NumericType, StringType,
  TypeAnnotation,
};
pub use parser::{parse_default_value, parse_idl_type, parse_idl_type_complete, ParseError};
pub use value::{
  DictionaryMemberSchema, DictionarySchema, TypeContext, WebIdlException, WebIdlValue,
};
