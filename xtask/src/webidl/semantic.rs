use super::ast::{Argument as AstArgument, BuiltinType as AstBuiltinType, IdlLiteral, IdlType as AstIdlType, InterfaceMember as AstInterfaceMember, SpecialOperation};
use super::parse_dictionary::parse_dictionary_member;
use super::parse_interface_member;
use super::resolve::{Exposure, ResolvedWebIdlWorld};
use super::{ExtendedAttribute};
use std::collections::{BTreeMap, BTreeSet};
use webidl_ir::{DictionaryMemberSchema, DictionarySchema, IdlType, NamedType, NamedTypeKind, TypeAnnotation, TypeContext};

#[derive(Debug, Default, Clone)]
pub struct SemanticWorld {
  pub interfaces: BTreeMap<String, SemanticInterface>,
  pub dictionaries: BTreeMap<String, SemanticDictionary>,
  pub enums: BTreeMap<String, SemanticEnum>,
  pub typedefs: BTreeMap<String, SemanticTypedef>,
  pub callbacks: BTreeMap<String, SemanticCallback>,
  pub diagnostics: Vec<SemanticDiagnostic>,
}

impl SemanticWorld {
  pub fn from_resolved(resolved: &ResolvedWebIdlWorld) -> Self {
    let mut diagnostics = Vec::<SemanticDiagnostic>::new();
    let mut unknown_named_types = BTreeSet::<String>::new();

    let enums = resolved
      .enums
      .values()
      .map(|en| {
        (
          en.name.clone(),
          SemanticEnum {
            name: en.name.clone(),
            ext_attrs: en.ext_attrs.clone(),
            values: en.values.clone(),
          },
        )
      })
      .collect::<BTreeMap<_, _>>();

    let typedefs = resolved
      .typedefs
      .values()
      .map(|td| {
        let mut parsed: Option<IdlType> = match webidl_ir::parse_idl_type_complete(&td.type_) {
          Ok(mut ty) => {
            resolve_named_types(
              &mut ty,
              resolved,
              &mut unknown_named_types,
              &mut diagnostics,
              &format!("typedef {}", td.name),
            );
            Some(ty)
          }
          Err(e) => {
            diagnostics.push(SemanticDiagnostic::TypedefParseFailed {
              name: td.name.clone(),
              raw: td.type_.clone(),
              error: e.to_string(),
            });
            None
          }
        };

        // Ensure determinism if parsing path later mutates.
        if let Some(ty) = &mut parsed {
          normalize_type(ty);
        }

        (
          td.name.clone(),
          SemanticTypedef {
            name: td.name.clone(),
            ext_attrs: td.ext_attrs.clone(),
            raw: td.type_.clone(),
            ty: parsed,
          },
        )
      })
      .collect::<BTreeMap<_, _>>();

    let callbacks = resolved
      .callbacks
      .values()
      .map(|cb| {
        let parsed = match parse_interface_member(&cb.type_) {
          Ok(AstInterfaceMember::Operation {
            name: None,
            return_type,
            arguments,
            ..
          }) => {
            let mut return_type = convert_type(
              &return_type,
              resolved,
              &mut unknown_named_types,
              &mut diagnostics,
              &format!("callback {} return type", cb.name),
            );
            normalize_type(&mut return_type);

            let args = arguments
              .iter()
              .map(|a| convert_argument(a, resolved, &mut unknown_named_types, &mut diagnostics, &cb.name))
              .collect();
            Some(SemanticCallbackFunction {
              return_type,
              arguments: args,
            })
          }
          Ok(other) => {
            diagnostics.push(SemanticDiagnostic::CallbackUnsupportedSyntax {
              name: cb.name.clone(),
              raw: cb.type_.clone(),
              parsed_as: format!("{other:?}"),
            });
            None
          }
          Err(e) => {
            diagnostics.push(SemanticDiagnostic::CallbackParseFailed {
              name: cb.name.clone(),
              raw: cb.type_.clone(),
              error: e.to_string(),
            });
            None
          }
        };

        (
          cb.name.clone(),
          SemanticCallback {
            name: cb.name.clone(),
            ext_attrs: cb.ext_attrs.clone(),
            raw: cb.type_.clone(),
            parsed,
          },
        )
      })
      .collect::<BTreeMap<_, _>>();

    let dictionaries = resolved
      .dictionaries
      .values()
      .map(|dict| {
        let mut members = Vec::<SemanticDictionaryMember>::new();
        for member in &dict.members {
          let raw_with_attrs = serialize_member_with_ext_attrs(&member.ext_attrs, &member.raw);
          match parse_dictionary_member(&raw_with_attrs) {
            Ok(mut parsed) => {
              resolve_named_types(
                &mut parsed.schema.ty,
                resolved,
                &mut unknown_named_types,
                &mut diagnostics,
                &format!("dictionary {} member {}", dict.name, parsed.schema.name),
              );
              normalize_type(&mut parsed.schema.ty);
              members.push(SemanticDictionaryMember {
                name: member.name.clone(),
                ext_attrs: member.ext_attrs.clone(),
                raw: member.raw.clone(),
                schema: Some(parsed.schema),
              });
            }
            Err(e) => {
              diagnostics.push(SemanticDiagnostic::DictionaryMemberParseFailed {
                dictionary: dict.name.clone(),
                raw: raw_with_attrs,
                error: e.to_string(),
              });
              members.push(SemanticDictionaryMember {
                name: member.name.clone(),
                ext_attrs: member.ext_attrs.clone(),
                raw: member.raw.clone(),
                schema: None,
              });
            }
          }
        }

        (
          dict.name.clone(),
          SemanticDictionary {
            name: dict.name.clone(),
            inherits: dict.inherits.clone(),
            ext_attrs: dict.ext_attrs.clone(),
            members,
          },
        )
      })
      .collect::<BTreeMap<_, _>>();

    let interfaces = resolved
      .interfaces
      .values()
      .map(|iface| {
        let mut members = Vec::<SemanticInterfaceMember>::new();
        for member in &iface.members {
          match parse_interface_member(&member.raw) {
            Ok(AstInterfaceMember::Unparsed { raw }) => {
              diagnostics.push(SemanticDiagnostic::InterfaceMemberUnsupportedSyntax {
                interface: iface.name.clone(),
                raw: raw.clone(),
              });
              members.push(SemanticInterfaceMember {
                name: member.name.clone(),
                ext_attrs: member.ext_attrs.clone(),
                exposure: member.exposure.clone(),
                raw: member.raw.clone(),
                parsed: None,
              });
            }
            Ok(ast) => {
              let parsed = convert_interface_member(
                &ast,
                resolved,
                &mut unknown_named_types,
                &mut diagnostics,
                &iface.name,
              );
              members.push(SemanticInterfaceMember {
                name: member.name.clone(),
                ext_attrs: member.ext_attrs.clone(),
                exposure: member.exposure.clone(),
                raw: member.raw.clone(),
                parsed,
              });
            }
            Err(e) => {
              diagnostics.push(SemanticDiagnostic::InterfaceMemberParseFailed {
                interface: iface.name.clone(),
                raw: member.raw.clone(),
                error: e.to_string(),
              });
              members.push(SemanticInterfaceMember {
                name: member.name.clone(),
                ext_attrs: member.ext_attrs.clone(),
                exposure: member.exposure.clone(),
                raw: member.raw.clone(),
                parsed: None,
              });
            }
          }
        }

        (
          iface.name.clone(),
          SemanticInterface {
            name: iface.name.clone(),
            inherits: iface.inherits.clone(),
            callback: iface.callback,
            ext_attrs: iface.ext_attrs.clone(),
            exposure: iface.exposure.clone(),
            members,
          },
        )
      })
      .collect::<BTreeMap<_, _>>();

    SemanticWorld {
      interfaces,
      dictionaries,
      enums,
      typedefs,
      callbacks,
      diagnostics,
    }
  }

  pub fn build_type_context(&self) -> TypeContext {
    let mut ctx = TypeContext::default();

    for en in self.enums.values() {
      ctx.add_enum(en.name.clone(), en.values.iter().cloned());
    }

    for dict in self.dictionaries.values() {
      let members = dict
        .members
        .iter()
        .filter_map(|m| m.schema.clone())
        .collect::<Vec<DictionaryMemberSchema>>();
      ctx.add_dictionary(DictionarySchema {
        name: dict.name.clone(),
        inherits: dict.inherits.clone(),
        members,
      });
    }

    for td in self.typedefs.values() {
      if let Some(ty) = &td.ty {
        ctx.add_typedef(td.name.clone(), ty.clone());
      }
    }

    ctx
  }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SemanticDiagnostic {
  InterfaceMemberParseFailed {
    interface: String,
    raw: String,
    error: String,
  },
  InterfaceMemberUnsupportedSyntax {
    interface: String,
    raw: String,
  },
  DictionaryMemberParseFailed {
    dictionary: String,
    raw: String,
    error: String,
  },
  TypedefParseFailed {
    name: String,
    raw: String,
    error: String,
  },
  CallbackParseFailed {
    name: String,
    raw: String,
    error: String,
  },
  CallbackUnsupportedSyntax {
    name: String,
    raw: String,
    parsed_as: String,
  },
  UnknownNamedType {
    name: String,
    first_seen_in: String,
  },
}

#[derive(Debug, Clone)]
pub struct SemanticInterface {
  pub name: String,
  pub inherits: Option<String>,
  pub callback: bool,
  pub ext_attrs: Vec<ExtendedAttribute>,
  pub exposure: Exposure,
  pub members: Vec<SemanticInterfaceMember>,
}

#[derive(Debug, Clone)]
pub struct SemanticInterfaceMember {
  pub name: Option<String>,
  pub ext_attrs: Vec<ExtendedAttribute>,
  pub exposure: Exposure,
  pub raw: String,
  pub parsed: Option<SemanticInterfaceMemberKind>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SemanticInterfaceMemberKind {
  Constructor { arguments: Vec<SemanticArgument> },
  Attribute {
    name: String,
    ty: IdlType,
    readonly: bool,
    inherit: bool,
    stringifier: bool,
    static_: bool,
  },
  Operation {
    name: Option<String>,
    return_type: IdlType,
    arguments: Vec<SemanticArgument>,
    static_: bool,
    stringifier: bool,
    special: Option<SpecialOperation>,
  },
  Constant { name: String, ty: IdlType, value: IdlLiteral },
  Iterable {
    async_: bool,
    key_type: Option<IdlType>,
    value_type: IdlType,
  },
}

#[derive(Debug, Clone, PartialEq)]
pub struct SemanticArgument {
  pub name: String,
  pub ext_attrs: Vec<ExtendedAttribute>,
  pub ty: IdlType,
  pub optional: bool,
  pub variadic: bool,
  pub default: Option<IdlLiteral>,
}

#[derive(Debug, Clone)]
pub struct SemanticDictionary {
  pub name: String,
  pub inherits: Option<String>,
  pub ext_attrs: Vec<ExtendedAttribute>,
  pub members: Vec<SemanticDictionaryMember>,
}

#[derive(Debug, Clone)]
pub struct SemanticDictionaryMember {
  pub name: Option<String>,
  pub ext_attrs: Vec<ExtendedAttribute>,
  pub raw: String,
  pub schema: Option<DictionaryMemberSchema>,
}

#[derive(Debug, Clone)]
pub struct SemanticEnum {
  pub name: String,
  pub ext_attrs: Vec<ExtendedAttribute>,
  pub values: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SemanticTypedef {
  pub name: String,
  pub ext_attrs: Vec<ExtendedAttribute>,
  pub raw: String,
  pub ty: Option<IdlType>,
}

#[derive(Debug, Clone)]
pub struct SemanticCallback {
  pub name: String,
  pub ext_attrs: Vec<ExtendedAttribute>,
  pub raw: String,
  pub parsed: Option<SemanticCallbackFunction>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SemanticCallbackFunction {
  pub return_type: IdlType,
  pub arguments: Vec<SemanticArgument>,
}

fn convert_interface_member(
  ast: &AstInterfaceMember,
  resolved: &ResolvedWebIdlWorld,
  unknown_named_types: &mut BTreeSet<String>,
  diagnostics: &mut Vec<SemanticDiagnostic>,
  interface_name: &str,
) -> Option<SemanticInterfaceMemberKind> {
  match ast {
    AstInterfaceMember::Constructor { arguments } => Some(SemanticInterfaceMemberKind::Constructor {
      arguments: arguments
        .iter()
        .map(|a| convert_argument(a, resolved, unknown_named_types, diagnostics, interface_name))
        .collect(),
    }),
    AstInterfaceMember::Attribute {
      name,
      type_,
      readonly,
      inherit,
      stringifier,
      static_,
    } => {
      let mut ty = convert_type(
        type_,
        resolved,
        unknown_named_types,
        diagnostics,
        &format!("interface {interface_name} attribute {name}"),
      );
      normalize_type(&mut ty);
      Some(SemanticInterfaceMemberKind::Attribute {
        name: name.clone(),
        ty,
        readonly: *readonly,
        inherit: *inherit,
        stringifier: *stringifier,
        static_: *static_,
      })
    }
    AstInterfaceMember::Operation {
      name,
      return_type,
      arguments,
      static_,
      stringifier,
      special,
    } => {
      let mut ret = convert_type(
        return_type,
        resolved,
        unknown_named_types,
        diagnostics,
        &format!(
          "interface {interface_name} operation {} return type",
          name.as_deref().unwrap_or("<anonymous>")
        ),
      );
      normalize_type(&mut ret);
      Some(SemanticInterfaceMemberKind::Operation {
        name: name.clone(),
        return_type: ret,
        arguments: arguments
          .iter()
          .map(|a| convert_argument(a, resolved, unknown_named_types, diagnostics, interface_name))
          .collect(),
        static_: *static_,
        stringifier: *stringifier,
        special: *special,
      })
    }
    AstInterfaceMember::Constant { name, type_, value } => {
      let mut ty = convert_type(
        type_,
        resolved,
        unknown_named_types,
        diagnostics,
        &format!("interface {interface_name} const {name}"),
      );
      normalize_type(&mut ty);
      Some(SemanticInterfaceMemberKind::Constant {
        name: name.clone(),
        ty,
        value: value.clone(),
      })
    }
    AstInterfaceMember::Iterable {
      async_,
      key_type,
      value_type,
    } => {
      let mut value_ty = convert_type(
        value_type,
        resolved,
        unknown_named_types,
        diagnostics,
        &format!("interface {interface_name} iterable value type"),
      );
      normalize_type(&mut value_ty);

      let key_ty = key_type.as_ref().map(|t| {
        let mut kt = convert_type(
          t,
          resolved,
          unknown_named_types,
          diagnostics,
          &format!("interface {interface_name} iterable key type"),
        );
        normalize_type(&mut kt);
        kt
      });

      Some(SemanticInterfaceMemberKind::Iterable {
        async_: *async_,
        key_type: key_ty,
        value_type: value_ty,
      })
    }
    AstInterfaceMember::Unparsed { .. } => None,
  }
}

fn convert_argument(
  arg: &AstArgument,
  resolved: &ResolvedWebIdlWorld,
  unknown_named_types: &mut BTreeSet<String>,
  diagnostics: &mut Vec<SemanticDiagnostic>,
  context: &str,
) -> SemanticArgument {
  let mut ty = convert_type(
    &arg.type_,
    resolved,
    unknown_named_types,
    diagnostics,
    &format!("{context} argument {}", arg.name),
  );
  let annotations = arg
    .ext_attrs
    .iter()
    .filter_map(type_annotation_from_ext_attr)
    .collect::<Vec<_>>();
  ty = apply_type_annotations(ty, annotations);
  normalize_type(&mut ty);

  SemanticArgument {
    name: arg.name.clone(),
    ext_attrs: arg.ext_attrs.clone(),
    ty,
    optional: arg.optional,
    variadic: arg.variadic,
    default: arg.default.clone(),
  }
}

fn convert_type(
  ty: &AstIdlType,
  resolved: &ResolvedWebIdlWorld,
  unknown_named_types: &mut BTreeSet<String>,
  diagnostics: &mut Vec<SemanticDiagnostic>,
  context: &str,
) -> IdlType {
  match ty {
    AstIdlType::Builtin(b) => convert_builtin_type(*b),
    AstIdlType::Named(name) => IdlType::Named(NamedType {
      name: name.clone(),
      kind: classify_named_type(name, resolved, unknown_named_types, diagnostics, context),
    }),
    AstIdlType::Nullable(inner) => IdlType::Nullable(Box::new(convert_type(
      inner,
      resolved,
      unknown_named_types,
      diagnostics,
      context,
    ))),
    AstIdlType::Union(members) => IdlType::Union(
      members
        .iter()
        .map(|m| convert_type(m, resolved, unknown_named_types, diagnostics, context))
        .collect(),
    ),
    AstIdlType::Sequence(inner) => IdlType::Sequence(Box::new(convert_type(
      inner,
      resolved,
      unknown_named_types,
      diagnostics,
      context,
    ))),
    AstIdlType::FrozenArray(inner) => IdlType::FrozenArray(Box::new(convert_type(
      inner,
      resolved,
      unknown_named_types,
      diagnostics,
      context,
    ))),
    AstIdlType::Promise(inner) => IdlType::Promise(Box::new(convert_type(
      inner,
      resolved,
      unknown_named_types,
      diagnostics,
      context,
    ))),
    AstIdlType::Record { key, value } => IdlType::Record(
      Box::new(convert_type(key, resolved, unknown_named_types, diagnostics, context)),
      Box::new(convert_type(
        value,
        resolved,
        unknown_named_types,
        diagnostics,
        context,
      )),
    ),
  }
}

fn convert_builtin_type(b: AstBuiltinType) -> IdlType {
  match b {
    AstBuiltinType::Undefined => IdlType::Undefined,
    AstBuiltinType::Any => IdlType::Any,
    AstBuiltinType::Boolean => IdlType::Boolean,
    AstBuiltinType::Byte => IdlType::Numeric(webidl_ir::NumericType::Byte),
    AstBuiltinType::Octet => IdlType::Numeric(webidl_ir::NumericType::Octet),
    AstBuiltinType::Short => IdlType::Numeric(webidl_ir::NumericType::Short),
    AstBuiltinType::UnsignedShort => IdlType::Numeric(webidl_ir::NumericType::UnsignedShort),
    AstBuiltinType::Long => IdlType::Numeric(webidl_ir::NumericType::Long),
    AstBuiltinType::UnsignedLong => IdlType::Numeric(webidl_ir::NumericType::UnsignedLong),
    AstBuiltinType::LongLong => IdlType::Numeric(webidl_ir::NumericType::LongLong),
    AstBuiltinType::UnsignedLongLong => IdlType::Numeric(webidl_ir::NumericType::UnsignedLongLong),
    AstBuiltinType::Float => IdlType::Numeric(webidl_ir::NumericType::Float),
    AstBuiltinType::UnrestrictedFloat => IdlType::Numeric(webidl_ir::NumericType::UnrestrictedFloat),
    AstBuiltinType::Double => IdlType::Numeric(webidl_ir::NumericType::Double),
    AstBuiltinType::UnrestrictedDouble => {
      IdlType::Numeric(webidl_ir::NumericType::UnrestrictedDouble)
    }
    AstBuiltinType::DOMString => IdlType::String(webidl_ir::StringType::DomString),
    AstBuiltinType::USVString => IdlType::String(webidl_ir::StringType::UsvString),
    AstBuiltinType::ByteString => IdlType::String(webidl_ir::StringType::ByteString),
    AstBuiltinType::Object => IdlType::Object,
  }
}

fn classify_named_type(
  name: &str,
  resolved: &ResolvedWebIdlWorld,
  unknown_named_types: &mut BTreeSet<String>,
  diagnostics: &mut Vec<SemanticDiagnostic>,
  context: &str,
) -> NamedTypeKind {
  let kind = if let Some(iface) = resolved.interfaces.get(name) {
    if iface.callback {
      NamedTypeKind::CallbackInterface
    } else {
      NamedTypeKind::Interface
    }
  } else if resolved.dictionaries.contains_key(name) {
    NamedTypeKind::Dictionary
  } else if resolved.enums.contains_key(name) {
    NamedTypeKind::Enum
  } else if resolved.typedefs.contains_key(name) {
    NamedTypeKind::Typedef
  } else if resolved.callbacks.contains_key(name) {
    NamedTypeKind::CallbackFunction
  } else {
    NamedTypeKind::Unresolved
  };

  if kind == NamedTypeKind::Unresolved && unknown_named_types.insert(name.to_string()) {
    diagnostics.push(SemanticDiagnostic::UnknownNamedType {
      name: name.to_string(),
      first_seen_in: context.to_string(),
    });
  }

  kind
}

fn resolve_named_types(
  ty: &mut IdlType,
  resolved: &ResolvedWebIdlWorld,
  unknown_named_types: &mut BTreeSet<String>,
  diagnostics: &mut Vec<SemanticDiagnostic>,
  context: &str,
) {
  match ty {
    IdlType::Named(NamedType { name, kind }) => {
      *kind = classify_named_type(name, resolved, unknown_named_types, diagnostics, context);
    }
    IdlType::Nullable(inner)
    | IdlType::Sequence(inner)
    | IdlType::FrozenArray(inner)
    | IdlType::AsyncSequence(inner)
    | IdlType::Promise(inner) => resolve_named_types(inner, resolved, unknown_named_types, diagnostics, context),
    IdlType::Union(members) => {
      for m in members {
        resolve_named_types(m, resolved, unknown_named_types, diagnostics, context);
      }
    }
    IdlType::Record(key, value) => {
      resolve_named_types(key, resolved, unknown_named_types, diagnostics, context);
      resolve_named_types(value, resolved, unknown_named_types, diagnostics, context);
    }
    IdlType::Annotated { inner, .. } => {
      resolve_named_types(inner, resolved, unknown_named_types, diagnostics, context);
    }
    IdlType::Any
    | IdlType::Undefined
    | IdlType::Boolean
    | IdlType::Numeric(_)
    | IdlType::BigInt
    | IdlType::String(_)
    | IdlType::Object
    | IdlType::Symbol => {}
  }
}

fn type_annotation_from_ext_attr(attr: &ExtendedAttribute) -> Option<TypeAnnotation> {
  match attr.name.as_str() {
    "Clamp" => Some(TypeAnnotation::Clamp),
    "EnforceRange" => Some(TypeAnnotation::EnforceRange),
    "LegacyNullToEmptyString" => Some(TypeAnnotation::LegacyNullToEmptyString),
    "LegacyTreatNonObjectAsNull" => Some(TypeAnnotation::LegacyTreatNonObjectAsNull),
    "AllowShared" => Some(TypeAnnotation::AllowShared),
    "AllowResizable" => Some(TypeAnnotation::AllowResizable),
    _ => None,
  }
}

fn apply_type_annotations(ty: IdlType, mut annotations: Vec<TypeAnnotation>) -> IdlType {
  if annotations.is_empty() {
    return ty;
  }

  match ty {
    IdlType::Annotated {
      annotations: existing,
      inner,
    } => {
      annotations.extend(existing);
      IdlType::Annotated {
        annotations,
        inner,
      }
    }
    other => IdlType::Annotated {
      annotations,
      inner: Box::new(other),
    },
  }
}

fn normalize_type(ty: &mut IdlType) {
  // We only normalize named-type kinds in-place today, but having a dedicated hook keeps the
  // semantic layer deterministic if we later add canonicalization passes.
  //
  // Current invariants:
  // - No reordering of union members (preserve source order for deterministic codegen diffs).
  // - Nested `Annotated` wrappers are flattened by merge logic in `apply_type_annotations`.
  let _ = ty;
}

fn serialize_member_with_ext_attrs(ext_attrs: &[ExtendedAttribute], raw: &str) -> String {
  if ext_attrs.is_empty() {
    return raw.to_string();
  }
  let mut list = String::new();
  for (idx, attr) in ext_attrs.iter().enumerate() {
    if idx > 0 {
      list.push_str(", ");
    }
    list.push_str(&attr.name);
    if let Some(v) = &attr.value {
      list.push('=');
      list.push_str(v);
    }
  }
  format!("[{list}] {raw}")
}
