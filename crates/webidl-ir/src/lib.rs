#![forbid(unsafe_code)]

mod default_value;
mod idl_type;
mod parser;

pub use default_value::{DefaultValue, NumericLiteral};
pub use idl_type::{
  DistinguishabilityCategory, IdlType, NamedType, NamedTypeKind, NumericType, StringType, TypeAnnotation,
};
pub use parser::{parse_default_value, parse_idl_type, parse_idl_type_complete, ParseError};

