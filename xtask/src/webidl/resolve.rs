//! WebIDL "resolution" pass: merge partial definitions and `includes` into a consolidated world.
//!
//! Determinism rules (important for codegen stability):
//!
//! - Interface members:
//!   1. Start with the primary interface definition members (in source order).
//!   2. Append `partial interface` members in file appearance order (each partial preserves its own
//!      member order).
//!   3. Apply `includes` statements in file appearance order by appending the (already-resolved)
//!      mixin members to the end of the target interface.
//! - Interface mixins / dictionaries follow the same "primary then partials appended" rule.

use super::{
  ExtendedAttribute, ParsedCallback, ParsedDefinition, ParsedDictionary, ParsedEnum, ParsedIncludes,
  ParsedInterface, ParsedInterfaceMixin, ParsedMember, ParsedTypedef, ParsedWebIdlWorld,
};
use super::{ast::IdlType, parse_idl_type};
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ExposureTarget {
  /// No filtering (equivalent to `*`).
  All,
  Window,
  Worker,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exposure {
  /// `[Exposed=*]`
  All,
  /// `[Exposed=(Window,Worker)]`, `[Exposed=Window]`, etc.
  Named(BTreeSet<String>),
  /// No `[Exposed]` present. We keep these when filtering to avoid silently dropping definitions.
  Unknown,
}

impl Exposure {
  pub fn matches(&self, target: ExposureTarget) -> bool {
    match target {
      ExposureTarget::All => true,
      ExposureTarget::Window => self.matches_name("Window"),
      ExposureTarget::Worker => self.matches_name("Worker"),
    }
  }

  fn matches_name(&self, name: &str) -> bool {
    match self {
      Exposure::All => true,
      Exposure::Named(names) => names.contains(name),
      Exposure::Unknown => true,
    }
  }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ResolvedWebIdlWorld {
  pub interfaces: BTreeMap<String, ResolvedInterface>,
  pub interface_mixins: BTreeMap<String, ResolvedInterfaceMixin>,
  pub dictionaries: BTreeMap<String, ResolvedDictionary>,
  pub enums: BTreeMap<String, ResolvedEnum>,
  pub typedefs: BTreeMap<String, ResolvedTypedef>,
  pub callbacks: BTreeMap<String, ResolvedCallback>,
}

impl ResolvedWebIdlWorld {
  pub fn interface(&self, name: &str) -> Option<&ResolvedInterface> {
    self.interfaces.get(name)
  }

  pub fn dictionary(&self, name: &str) -> Option<&ResolvedDictionary> {
    self.dictionaries.get(name)
  }

  /// Resolve a `typedef` name to its fully expanded (canonicalized) type.
  ///
  /// This follows typedef chains recursively and errors on cycles.
  pub fn resolve_typedef(&self, name: &str) -> Result<IdlType> {
    #[derive(Default)]
    struct Ctx {
      in_progress: BTreeSet<String>,
      stack: Vec<String>,
      cache: BTreeMap<String, IdlType>,
    }

    fn inner(world: &ResolvedWebIdlWorld, name: &str, ctx: &mut Ctx) -> Result<IdlType> {
      if let Some(cached) = ctx.cache.get(name) {
        return Ok(cached.clone());
      }

      let td = world
        .typedefs
        .get(name)
        .with_context(|| format!("unknown typedef `{name}`"))?;

      if !ctx.in_progress.insert(name.to_string()) {
        let start = ctx
          .stack
          .iter()
          .position(|n| n == name)
          .unwrap_or(0);
        let mut cycle: Vec<String> = ctx.stack[start..].to_vec();
        cycle.push(name.to_string());
        bail!("typedef cycle detected: {}", cycle.join(" -> "));
      }

      ctx.stack.push(name.to_string());

      let parsed =
        parse_idl_type(&td.type_).with_context(|| format!("parse typedef `{name}` = `{}`", td.type_))?;

      // Canonicalize the typedef body, recursively expanding any referenced typedefs.
      let resolved = parsed.canonicalize_with(&mut |ref_name| {
        if world.typedefs.contains_key(ref_name) {
          Ok(Some(inner(world, ref_name, ctx)?))
        } else {
          Ok(None)
        }
      })?;

      ctx.stack.pop();
      ctx.in_progress.remove(name);
      ctx.cache.insert(name.to_string(), resolved.clone());
      Ok(resolved)
    }

    let mut ctx = Ctx::default();
    inner(self, name, &mut ctx)
  }

  /// Returns a shallowly filtered world for the given target environment.
  ///
  /// Rules:
  /// - Definitions/members with exposure `Unknown` are retained.
  /// - `typedef`s/`enum`s are retained unconditionally for now (they're used for type references).
  pub fn filter_by_exposure(&self, target: ExposureTarget) -> ResolvedWebIdlWorld {
    if target == ExposureTarget::All {
      return self.clone();
    }

    let mut out = self.clone();
    out.interfaces.retain(|_, iface| iface.exposure.matches(target));
    for iface in out.interfaces.values_mut() {
      iface.members.retain(|m| m.exposure.matches(target));
    }
    out
  }

  /// Returns flattened dictionary members (including inherited members, if available).
  ///
  /// Ordering: base dictionary members first, then derived members.
  pub fn flattened_dictionary_members(&self, name: &str) -> Vec<ResolvedDictionaryMember> {
    let mut out = Vec::new();
    let mut visited = BTreeSet::<String>::new();
    self.flattened_dictionary_members_inner(name, &mut visited, &mut out);
    out
  }

  fn flattened_dictionary_members_inner(
    &self,
    name: &str,
    visited: &mut BTreeSet<String>,
    out: &mut Vec<ResolvedDictionaryMember>,
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedInterface {
  pub name: String,
  pub inherits: Option<String>,
  pub ext_attrs: Vec<ExtendedAttribute>,
  pub exposure: Exposure,
  pub members: Vec<ResolvedInterfaceMember>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedInterfaceMember {
  pub name: Option<String>,
  pub ext_attrs: Vec<ExtendedAttribute>,
  pub exposure: Exposure,
  pub raw: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedInterfaceMixin {
  pub name: String,
  pub ext_attrs: Vec<ExtendedAttribute>,
  pub members: Vec<ResolvedInterfaceMember>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedDictionary {
  pub name: String,
  pub inherits: Option<String>,
  pub ext_attrs: Vec<ExtendedAttribute>,
  pub members: Vec<ResolvedDictionaryMember>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedDictionaryMember {
  pub name: Option<String>,
  pub ext_attrs: Vec<ExtendedAttribute>,
  pub raw: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedEnum {
  pub name: String,
  pub ext_attrs: Vec<ExtendedAttribute>,
  pub values: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedTypedef {
  pub name: String,
  pub ext_attrs: Vec<ExtendedAttribute>,
  pub type_: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedCallback {
  pub name: String,
  pub ext_attrs: Vec<ExtendedAttribute>,
  pub type_: String,
}

pub fn resolve_webidl_world(parsed: &ParsedWebIdlWorld) -> ResolvedWebIdlWorld {
  let mut primary_interfaces: BTreeMap<String, ParsedInterface> = BTreeMap::new();
  let mut partial_interfaces: BTreeMap<String, Vec<ParsedInterface>> = BTreeMap::new();

  let mut primary_mixins: BTreeMap<String, ParsedInterfaceMixin> = BTreeMap::new();
  let mut partial_mixins: BTreeMap<String, Vec<ParsedInterfaceMixin>> = BTreeMap::new();

  let mut primary_dicts: BTreeMap<String, ParsedDictionary> = BTreeMap::new();
  let mut partial_dicts: BTreeMap<String, Vec<ParsedDictionary>> = BTreeMap::new();

  let mut includes: Vec<ParsedIncludes> = Vec::new();
  let mut enums: BTreeMap<String, ParsedEnum> = BTreeMap::new();
  let mut typedefs: BTreeMap<String, ParsedTypedef> = BTreeMap::new();
  let mut callbacks: BTreeMap<String, ParsedCallback> = BTreeMap::new();

  for def in &parsed.definitions {
    match def {
      ParsedDefinition::Interface(iface) => {
        if iface.partial {
          partial_interfaces
            .entry(iface.name.clone())
            .or_default()
            .push(iface.clone());
        } else {
          primary_interfaces.entry(iface.name.clone()).or_insert(iface.clone());
        }
      }
      ParsedDefinition::InterfaceMixin(mixin) => {
        if mixin.partial {
          partial_mixins
            .entry(mixin.name.clone())
            .or_default()
            .push(mixin.clone());
        } else {
          primary_mixins.entry(mixin.name.clone()).or_insert(mixin.clone());
        }
      }
      ParsedDefinition::Includes(i) => includes.push(i.clone()),
      ParsedDefinition::Dictionary(dict) => {
        if dict.partial {
          partial_dicts
            .entry(dict.name.clone())
            .or_default()
            .push(dict.clone());
        } else {
          primary_dicts.entry(dict.name.clone()).or_insert(dict.clone());
        }
      }
      ParsedDefinition::Enum(e) => {
        enums.insert(e.name.clone(), e.clone());
      }
      ParsedDefinition::Typedef(td) => {
        typedefs.insert(td.name.clone(), td.clone());
      }
      ParsedDefinition::Callback(cb) => {
        callbacks.insert(cb.name.clone(), cb.clone());
      }
      ParsedDefinition::Other { .. } => {}
    }
  }

  let interface_mixins = resolve_mixins(primary_mixins, partial_mixins);
  let mut interfaces = resolve_interfaces(primary_interfaces, partial_interfaces);

  // Apply `includes` to interfaces in file appearance order.
  for inc in includes {
    let Some(mixin) = interface_mixins.get(&inc.mixin) else {
      continue;
    };
    let Some(target) = interfaces.get_mut(&inc.target) else {
      continue;
    };
    target.members.extend(mixin.members.iter().cloned());
  }

  // After includes, compute member exposures based on (member || interface) Exposed=.
  for iface in interfaces.values_mut() {
    for member in &mut iface.members {
      member.exposure = effective_member_exposure(&member.ext_attrs, &iface.exposure);
    }
  }

  let dictionaries = resolve_dictionaries(primary_dicts, partial_dicts);

  ResolvedWebIdlWorld {
    interfaces,
    interface_mixins,
    dictionaries,
    enums: enums
      .into_values()
      .map(|e| {
        (
          e.name.clone(),
          ResolvedEnum {
            name: e.name,
            ext_attrs: e.ext_attrs,
            values: e.values,
          },
        )
      })
      .collect(),
    typedefs: typedefs
      .into_values()
      .map(|td| {
        (
          td.name.clone(),
          ResolvedTypedef {
            name: td.name,
            ext_attrs: td.ext_attrs,
            type_: td.type_,
          },
        )
      })
      .collect(),
    callbacks: callbacks
      .into_values()
      .map(|cb| {
        (
          cb.name.clone(),
          ResolvedCallback {
            name: cb.name,
            ext_attrs: cb.ext_attrs,
            type_: cb.type_,
          },
        )
      })
      .collect(),
  }
}

fn resolve_mixins(
  primary: BTreeMap<String, ParsedInterfaceMixin>,
  mut partials: BTreeMap<String, Vec<ParsedInterfaceMixin>>,
) -> BTreeMap<String, ResolvedInterfaceMixin> {
  let mut out: BTreeMap<String, ResolvedInterfaceMixin> = BTreeMap::new();

  let mut all_names: BTreeSet<String> = primary.keys().cloned().collect();
  all_names.extend(partials.keys().cloned());

  for name in all_names {
    let mut ext_attrs = Vec::new();
    let mut members = Vec::new();

    if let Some(base) = primary.get(&name) {
      ext_attrs.extend(base.ext_attrs.clone());
      members.extend(base.members.iter().map(|m| to_resolved_iface_member(m, &Exposure::Unknown)));
    }

    if let Some(ps) = partials.remove(&name) {
      for p in ps {
        ext_attrs.extend(p.ext_attrs);
        members.extend(p.members.iter().map(|m| to_resolved_iface_member(m, &Exposure::Unknown)));
      }
    }

    out.insert(
      name.clone(),
      ResolvedInterfaceMixin {
        name,
        ext_attrs,
        members,
      },
    );
  }

  out
}

fn resolve_interfaces(
  primary: BTreeMap<String, ParsedInterface>,
  mut partials: BTreeMap<String, Vec<ParsedInterface>>,
) -> BTreeMap<String, ResolvedInterface> {
  let mut out: BTreeMap<String, ResolvedInterface> = BTreeMap::new();
  let mut all_names: BTreeSet<String> = primary.keys().cloned().collect();
  all_names.extend(partials.keys().cloned());

  for name in all_names {
    let mut inherits = None;
    let mut ext_attrs = Vec::new();
    let mut members = Vec::new();

    if let Some(base) = primary.get(&name) {
      inherits = base.inherits.clone();
      ext_attrs.extend(base.ext_attrs.clone());
      members.extend(base.members.iter().map(|m| to_resolved_iface_member(m, &Exposure::Unknown)));
    }

    if let Some(ps) = partials.remove(&name) {
      for p in ps {
        if inherits.is_none() {
          inherits = p.inherits;
        }
        ext_attrs.extend(p.ext_attrs);
        members.extend(p.members.iter().map(|m| to_resolved_iface_member(m, &Exposure::Unknown)));
      }
    }

    let exposure = exposure_from_ext_attrs(&ext_attrs);
    // Patch members with effective exposure after we know interface exposure.
    for m in &mut members {
      m.exposure = effective_member_exposure(&m.ext_attrs, &exposure);
    }

    out.insert(
      name.clone(),
      ResolvedInterface {
        name,
        inherits,
        ext_attrs,
        exposure,
        members,
      },
    );
  }

  out
}

fn resolve_dictionaries(
  primary: BTreeMap<String, ParsedDictionary>,
  mut partials: BTreeMap<String, Vec<ParsedDictionary>>,
) -> BTreeMap<String, ResolvedDictionary> {
  let mut out: BTreeMap<String, ResolvedDictionary> = BTreeMap::new();
  let mut all_names: BTreeSet<String> = primary.keys().cloned().collect();
  all_names.extend(partials.keys().cloned());

  for name in all_names {
    let mut inherits = None;
    let mut ext_attrs = Vec::new();
    let mut members = Vec::new();

    if let Some(base) = primary.get(&name) {
      inherits = base.inherits.clone();
      ext_attrs.extend(base.ext_attrs.clone());
      members.extend(base.members.iter().map(to_resolved_dict_member));
    }

    if let Some(ps) = partials.remove(&name) {
      for p in ps {
        if inherits.is_none() {
          inherits = p.inherits;
        }
        ext_attrs.extend(p.ext_attrs);
        members.extend(p.members.iter().map(to_resolved_dict_member));
      }
    }

    out.insert(
      name.clone(),
      ResolvedDictionary {
        name,
        inherits,
        ext_attrs,
        members,
      },
    );
  }

  out
}

fn to_resolved_iface_member(m: &ParsedMember, parent_exposure: &Exposure) -> ResolvedInterfaceMember {
  ResolvedInterfaceMember {
    name: m.name.clone(),
    ext_attrs: m.ext_attrs.clone(),
    exposure: effective_member_exposure(&m.ext_attrs, parent_exposure),
    raw: m.raw.clone(),
  }
}

fn to_resolved_dict_member(m: &ParsedMember) -> ResolvedDictionaryMember {
  ResolvedDictionaryMember {
    name: m.name.clone(),
    ext_attrs: m.ext_attrs.clone(),
    raw: m.raw.clone(),
  }
}

fn effective_member_exposure(member_attrs: &[ExtendedAttribute], parent: &Exposure) -> Exposure {
  let declared = exposure_from_ext_attrs(member_attrs);
  match declared {
    Exposure::Unknown => parent.clone(),
    other => other,
  }
}

pub fn exposure_from_ext_attrs(attrs: &[ExtendedAttribute]) -> Exposure {
  let Some(attr) = attrs.iter().find(|a| a.name == "Exposed") else {
    return Exposure::Unknown;
  };
  let Some(value) = &attr.value else {
    return Exposure::Unknown;
  };

  let v = value.trim();
  if v == "*" {
    return Exposure::All;
  }

  let mut names = BTreeSet::new();
  let inner = v
    .strip_prefix('(')
    .and_then(|s| s.strip_suffix(')'))
    .unwrap_or(v);
  for seg in inner.split(',') {
    let name = seg.trim();
    if !name.is_empty() {
      names.insert(name.to_string());
    }
  }
  if names.is_empty() {
    Exposure::Unknown
  } else {
    Exposure::Named(names)
  }
}
