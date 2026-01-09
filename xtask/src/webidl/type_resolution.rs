//! WebIDL type resolution helpers bridging the `xtask` WebIDL snapshot/resolver and `webidl-ir`.
//!
//! The `xtask::webidl` parser stores most signatures/member bodies as raw text and uses a
//! lightweight `ast::IdlType` for parsing a small subset of WebIDL. For bindings/codegen we need
//! richer, spec-shaped type information (type annotations, BigInt/Symbol, named-type categories,
//! typedef expansion).
//!
//! This module:
//! - builds a `webidl_ir::TypeContext` from a resolved WebIDL world, and
//! - provides helpers to parse/resolve WebIDL type strings into `webidl_ir::IdlType`.

use super::resolve::{ResolvedDictionaryMember, ResolvedWebIdlWorld};
use super::ExtendedAttribute;
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use webidl_ir::{
  parse_default_value, parse_idl_type, parse_idl_type_complete, DictionaryMemberSchema,
  DictionarySchema, DefaultValue, IdlType, NamedType, NamedTypeKind, TypeAnnotation, TypeContext,
};

/// Build a `webidl_ir::TypeContext` from a resolved WebIDL world.
///
/// Deterministic ordering:
/// - maps are stored as `BTreeMap`/`BTreeSet`, and
/// - dictionary members preserve appearance order from the `ResolvedWebIdlWorld`.
pub fn build_type_context(world: &ResolvedWebIdlWorld) -> Result<TypeContext> {
  let mut ctx = TypeContext::default();

  // Enums.
  for en in world.enums.values() {
    ctx.add_enum(&en.name, en.values.iter().cloned());
  }

  // Typedefs (must be available for dictionary member types/default evaluation).
  for td in world.typedefs.values() {
    let ty = parse_type_with_world(world, &td.type_, &td.ext_attrs)?;
    ctx.add_typedef(&td.name, ty);
  }

  // Dictionaries.
  for dict in world.dictionaries.values() {
    let mut members = Vec::<DictionaryMemberSchema>::with_capacity(dict.members.len());
    for member in &dict.members {
      let schema =
        parse_dictionary_member_schema(world, &ctx, member).with_context(|| {
          format!(
            "parse dictionary member in `{}`: `{}`",
            dict.name, member.raw
          )
        })?;
      members.push(schema);
    }

    ctx.add_dictionary(DictionarySchema {
      name: dict.name.clone(),
      inherits: dict.inherits.clone(),
      members,
    });
  }

  Ok(ctx)
}

/// Parse a WebIDL type string and resolve any named-type categories.
///
/// `extra_annotations` are WebIDL extended attributes that were syntactically associated with the
/// type (e.g. `[Clamp]` on a dictionary member) but may have been extracted separately by the
/// upstream `xtask::webidl` parser.
pub fn parse_type_with_world(
  world: &ResolvedWebIdlWorld,
  input: &str,
  extra_annotations: &[ExtendedAttribute],
) -> Result<IdlType> {
  let mut ty =
    parse_idl_type_complete(input).map_err(|e| anyhow::anyhow!("{e}")).with_context(|| {
      format!("parse WebIDL type `{}`", input.trim())
    })?;

  if !extra_annotations.is_empty() {
    ty = merge_extra_annotations(ty, extra_annotations);
  }

  resolve_named_type_kinds_in_place(&mut ty, world);
  Ok(ty)
}

/// Parse a WebIDL type string, resolve named-type categories, and optionally expand typedefs.
pub fn parse_type_with_world_and_typedefs(
  world: &ResolvedWebIdlWorld,
  ctx: &TypeContext,
  input: &str,
  extra_annotations: &[ExtendedAttribute],
  expand_typedefs: bool,
) -> Result<IdlType> {
  let ty = parse_type_with_world(world, input, extra_annotations)?;
  if expand_typedefs {
    expand_typedefs_in_type(ctx, &ty)
  } else {
    Ok(ty)
  }
}

/// Parse a resolved dictionary member into a `webidl_ir::DictionaryMemberSchema`.
pub fn parse_dictionary_member_schema(
  world: &ResolvedWebIdlWorld,
  _ctx: &TypeContext,
  member: &ResolvedDictionaryMember,
) -> Result<DictionaryMemberSchema> {
  // `ResolvedDictionaryMember.raw` already has leading extended attributes stripped into
  // `ResolvedDictionaryMember.ext_attrs`.
  let mut s = member.raw.trim();
  let required = consume_keyword(&mut s, "required");

  let (ty, rest) = parse_idl_type(s)
    .map_err(|e| anyhow::anyhow!("{e}"))
    .with_context(|| format!("parse dictionary member type in `{}`", member.raw))?;
  let mut ty = merge_extra_annotations(ty, &member.ext_attrs);
  resolve_named_type_kinds_in_place(&mut ty, world);

  let rest = rest.trim_start();
  let (name, rest) = parse_identifier_prefix(rest)
    .with_context(|| format!("parse dictionary member name in `{}`", member.raw))?;

  let mut rest = rest.trim_start();
  let default = if rest.starts_with('=') {
    rest = &rest[1..];
    let dv_text = rest.trim_start();
    if dv_text.is_empty() {
      bail!("dictionary member has `=` but no default value: `{}`", member.raw);
    }
    let dv = parse_default_value(dv_text)
      .map_err(|e| anyhow::anyhow!("{e}"))
      .with_context(|| format!("parse default value `{}`", dv_text))?;
    Some(dv)
  } else {
    None
  };

  // Fail fast on unexpected trailing tokens when no default is present.
  if default.is_none() && !rest.trim().is_empty() {
    bail!(
      "unexpected trailing tokens after dictionary member name: `{}`",
      rest.trim()
    );
  }

  Ok(DictionaryMemberSchema {
    name: name.to_string(),
    required,
    ty,
    default,
  })
}

fn resolve_named_type_kinds_in_place(ty: &mut IdlType, world: &ResolvedWebIdlWorld) {
  match ty {
    IdlType::Named(NamedType { name, kind }) => {
      *kind = kind_for_name(world, name);
    }
    IdlType::Nullable(inner)
    | IdlType::Sequence(inner)
    | IdlType::FrozenArray(inner)
    | IdlType::AsyncSequence(inner)
    | IdlType::Promise(inner) => resolve_named_type_kinds_in_place(inner, world),
    IdlType::Union(members) => {
      for m in members {
        resolve_named_type_kinds_in_place(m, world);
      }
    }
    IdlType::Record(key, value) => {
      resolve_named_type_kinds_in_place(key, world);
      resolve_named_type_kinds_in_place(value, world);
    }
    IdlType::Annotated { inner, .. } => resolve_named_type_kinds_in_place(inner, world),
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

fn kind_for_name(world: &ResolvedWebIdlWorld, name: &str) -> NamedTypeKind {
  if let Some(iface) = world.interfaces.get(name) {
    if iface.callback {
      return NamedTypeKind::CallbackInterface;
    }
    return NamedTypeKind::Interface;
  }
  if world.dictionaries.contains_key(name) {
    return NamedTypeKind::Dictionary;
  }
  if world.enums.contains_key(name) {
    return NamedTypeKind::Enum;
  }
  if world.typedefs.contains_key(name) {
    return NamedTypeKind::Typedef;
  }
  if world.callbacks.contains_key(name) {
    return NamedTypeKind::CallbackFunction;
  }
  NamedTypeKind::Unresolved
}

fn merge_extra_annotations(ty: IdlType, extra: &[ExtendedAttribute]) -> IdlType {
  if extra.is_empty() {
    return ty;
  }
  let mut annotations = extra.iter().map(ext_attr_to_type_annotation).collect::<Vec<_>>();

  match ty {
    IdlType::Annotated {
      annotations: mut existing,
      inner,
    } => {
      annotations.append(&mut existing);
      IdlType::Annotated { annotations, inner }
    }
    other => IdlType::Annotated {
      annotations,
      inner: Box::new(other),
    },
  }
}

fn ext_attr_to_type_annotation(attr: &ExtendedAttribute) -> TypeAnnotation {
  match attr.name.as_str() {
    "Clamp" => TypeAnnotation::Clamp,
    "EnforceRange" => TypeAnnotation::EnforceRange,
    "LegacyNullToEmptyString" => TypeAnnotation::LegacyNullToEmptyString,
    "LegacyTreatNonObjectAsNull" => TypeAnnotation::LegacyTreatNonObjectAsNull,
    "AllowShared" => TypeAnnotation::AllowShared,
    "AllowResizable" => TypeAnnotation::AllowResizable,
    other => TypeAnnotation::Other {
      name: other.to_string(),
      rhs: attr.value.clone(),
    },
  }
}

fn consume_keyword<'a>(s: &mut &'a str, kw: &str) -> bool {
  let rest = s.trim_start();
  if !rest.starts_with(kw) {
    return false;
  }
  let after = &rest[kw.len()..];
  if after
    .chars()
    .next()
    .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
  {
    return false;
  }
  *s = after;
  true
}

fn parse_identifier_prefix(s: &str) -> Result<(&str, &str)> {
  let s = s.trim_start();
  let bytes = s.as_bytes();
  let mut i = 0usize;
  while i < bytes.len() {
    let b = bytes[i];
    let ok = if i == 0 {
      b.is_ascii_alphabetic() || b == b'_'
    } else {
      b.is_ascii_alphanumeric() || b == b'_'
    };
    if !ok {
      break;
    }
    i += 1;
  }
  if i == 0 {
    bail!("expected identifier");
  }
  Ok((&s[..i], &s[i..]))
}

fn expand_typedefs_in_type(ctx: &TypeContext, ty: &IdlType) -> Result<IdlType> {
  #[derive(Default)]
  struct Ctx {
    in_progress: BTreeSet<String>,
    stack: Vec<String>,
    cache: BTreeMap<String, IdlType>,
  }

  fn resolve_typedef(ctx: &TypeContext, name: &str, state: &mut Ctx) -> Result<IdlType> {
    if let Some(cached) = state.cache.get(name) {
      return Ok(cached.clone());
    }

    let body = ctx
      .typedefs
      .get(name)
      .with_context(|| format!("unknown typedef `{name}`"))?;

    if !state.in_progress.insert(name.to_string()) {
      let start = state
        .stack
        .iter()
        .position(|n| n == name)
        .unwrap_or(0);
      let mut cycle: Vec<String> = state.stack[start..].to_vec();
      cycle.push(name.to_string());
      bail!("typedef cycle detected: {}", cycle.join(" -> "));
    }

    state.stack.push(name.to_string());
    let resolved = expand_type(ctx, body, state)?;
    state.stack.pop();
    state.in_progress.remove(name);
    state.cache.insert(name.to_string(), resolved.clone());
    Ok(resolved)
  }

  fn expand_type(ctx: &TypeContext, ty: &IdlType, state: &mut Ctx) -> Result<IdlType> {
    Ok(match ty {
      IdlType::Named(NamedType { name, .. }) if ctx.typedefs.contains_key(name) => {
        resolve_typedef(ctx, name, state)?
      }

      IdlType::Nullable(inner) => IdlType::Nullable(Box::new(expand_type(ctx, inner, state)?)),
      IdlType::Union(members) => {
        let mut out = Vec::with_capacity(members.len());
        for m in members {
          out.push(expand_type(ctx, m, state)?);
        }
        IdlType::Union(out)
      }
      IdlType::Sequence(inner) => IdlType::Sequence(Box::new(expand_type(ctx, inner, state)?)),
      IdlType::FrozenArray(inner) => {
        IdlType::FrozenArray(Box::new(expand_type(ctx, inner, state)?))
      }
      IdlType::AsyncSequence(inner) => {
        IdlType::AsyncSequence(Box::new(expand_type(ctx, inner, state)?))
      }
      IdlType::Record(key, value) => IdlType::Record(
        Box::new(expand_type(ctx, key, state)?),
        Box::new(expand_type(ctx, value, state)?),
      ),
      IdlType::Promise(inner) => IdlType::Promise(Box::new(expand_type(ctx, inner, state)?)),
      IdlType::Annotated { annotations, inner } => IdlType::Annotated {
        annotations: annotations.clone(),
        inner: Box::new(expand_type(ctx, inner, state)?),
      },

      other => other.clone(),
    })
  }

  let mut state = Ctx::default();
  expand_type(ctx, ty, &mut state)
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::webidl::overload_ir;

  fn test_world() -> ResolvedWebIdlWorld {
    let idl = r#"
      enum E { "a", "b" };

      typedef DOMString Foo;
      typedef Foo Bar;

      callback Cb = undefined();

      interface I {};
      callback interface CI {};

      dictionary Base {
        required DOMString base;
        [Clamp] unsigned long x = 1;
      };

      dictionary Derived : Base {
        boolean y = true;
      };
    "#;
    let parsed = crate::webidl::parse_webidl(idl).expect("parse");
    crate::webidl::resolve::resolve_webidl_world(&parsed)
  }

  #[test]
  fn build_type_context_typedef_enum_dict_inheritance() {
    let world = test_world();
    let ctx = build_type_context(&world).expect("build type context");

    assert_eq!(
      ctx.enums.get("E").cloned(),
      Some(BTreeSet::from(["a".to_string(), "b".to_string()]))
    );

    assert_eq!(
      ctx.typedefs.get("Foo").cloned(),
      Some(IdlType::String(webidl_ir::StringType::DomString))
    );
    assert_eq!(
      ctx.typedefs.get("Bar").cloned(),
      Some(IdlType::Named(NamedType {
        name: "Foo".to_string(),
        kind: NamedTypeKind::Typedef,
      }))
    );

    let derived = ctx.dictionaries.get("Derived").expect("Derived dict");
    assert_eq!(derived.inherits.as_deref(), Some("Base"));
  }

  #[test]
  fn parse_dictionary_members_required_default_and_flattening_order() {
    let world = test_world();
    let ctx = build_type_context(&world).expect("build type context");

    let flattened = ctx
      .flattened_dictionary_members("Derived")
      .expect("flattened members");

    let names = flattened
      .iter()
      .map(|m| m.name.as_str())
      .collect::<Vec<_>>();
    assert_eq!(names, vec!["base", "x", "y"]);

    assert!(flattened[0].required);
    assert_eq!(
      flattened[0].ty,
      IdlType::String(webidl_ir::StringType::DomString)
    );
    assert_eq!(flattened[0].default, None);

    assert!(!flattened[1].required);
    assert_eq!(
      flattened[1].ty,
      IdlType::Annotated {
        annotations: vec![TypeAnnotation::Clamp],
        inner: Box::new(IdlType::Numeric(webidl_ir::NumericType::UnsignedLong)),
      }
    );
    assert_eq!(
      flattened[1].default,
      Some(DefaultValue::Number(webidl_ir::NumericLiteral::Integer(
        "1".to_string()
      )))
    );

    assert!(!flattened[2].required);
    assert_eq!(flattened[2].ty, IdlType::Boolean);
    assert_eq!(flattened[2].default, Some(DefaultValue::Boolean(true)));
  }

  #[test]
  fn map_parsed_types_into_overload_ir_types() {
    let world = test_world();
    let ctx = build_type_context(&world).expect("build type context");

    // Typedef expansion.
    let ty = parse_type_with_world_and_typedefs(&world, &ctx, "Bar", &[], true).unwrap();
    assert_eq!(ty, IdlType::String(webidl_ir::StringType::DomString));

    // Callback function + annotation (legacy treat non-object as null affects distinguishability
    // vs dictionary types in WebIDL's table).
    let cb_ty = parse_type_with_world(&world, "[LegacyTreatNonObjectAsNull] Cb", &[]).unwrap();
    assert_eq!(
      cb_ty,
      IdlType::Annotated {
        annotations: vec![TypeAnnotation::LegacyTreatNonObjectAsNull],
        inner: Box::new(IdlType::Named(NamedType {
          name: "Cb".to_string(),
          kind: NamedTypeKind::CallbackFunction,
        })),
      }
    );

    // Feed resolved types into overload validation.
    let overloads = vec![
      overload_ir::Overload {
        name: "f".to_string(),
        arguments: vec![overload_ir::OverloadArgument::required(ty.clone())],
        origin: None,
      },
      overload_ir::Overload {
        name: "f".to_string(),
        arguments: vec![overload_ir::OverloadArgument::required(IdlType::Boolean)],
        origin: None,
      },
    ];
    overload_ir::validate_overload_set(&overloads, &world).expect("valid overload set");

    let dict_ty = parse_type_with_world(&world, "Derived", &[]).unwrap();
    assert!(
      !overload_ir::are_distinguishable(&cb_ty, &dict_ty, &world),
      "callback function with LegacyTreatNonObjectAsNull must not be distinguishable from a dictionary"
    );
  }
}
