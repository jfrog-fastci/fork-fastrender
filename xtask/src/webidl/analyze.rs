use super::ast::InterfaceMember;
use super::parse::parse_interface_member;
use super::resolve::{
  Exposure, ResolvedCallback, ResolvedDictionary, ResolvedEnum, ResolvedInterface,
  ResolvedInterfaceMember, ResolvedInterfaceMixin, ResolvedTypedef, ResolvedWebIdlWorld,
};
use super::ExtendedAttribute;
use std::collections::BTreeMap;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AnalyzedWebIdlWorld {
  pub interfaces: BTreeMap<String, AnalyzedInterface>,
  pub interface_mixins: BTreeMap<String, AnalyzedInterfaceMixin>,
  pub dictionaries: BTreeMap<String, ResolvedDictionary>,
  pub enums: BTreeMap<String, ResolvedEnum>,
  pub typedefs: BTreeMap<String, ResolvedTypedef>,
  pub callbacks: BTreeMap<String, ResolvedCallback>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalyzedInterface {
  pub name: String,
  pub inherits: Option<String>,
  pub ext_attrs: Vec<ExtendedAttribute>,
  pub exposure: Exposure,
  pub members: Vec<AnalyzedInterfaceMember>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalyzedInterfaceMixin {
  pub name: String,
  pub ext_attrs: Vec<ExtendedAttribute>,
  pub members: Vec<AnalyzedInterfaceMember>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnalyzedInterfaceMember {
  pub name: Option<String>,
  pub ext_attrs: Vec<ExtendedAttribute>,
  pub exposure: Exposure,
  pub raw: String,
  pub parsed: InterfaceMember,
}

pub fn analyze_resolved_world(resolved: &ResolvedWebIdlWorld) -> AnalyzedWebIdlWorld {
  let interfaces = resolved
    .interfaces
    .iter()
    .map(|(name, iface)| (name.clone(), analyze_interface(iface)))
    .collect();

  let interface_mixins = resolved
    .interface_mixins
    .iter()
    .map(|(name, mixin)| (name.clone(), analyze_mixin(mixin)))
    .collect();

  AnalyzedWebIdlWorld {
    interfaces,
    interface_mixins,
    dictionaries: resolved.dictionaries.clone(),
    enums: resolved.enums.clone(),
    typedefs: resolved.typedefs.clone(),
    callbacks: resolved.callbacks.clone(),
  }
}

fn analyze_interface(iface: &ResolvedInterface) -> AnalyzedInterface {
  AnalyzedInterface {
    name: iface.name.clone(),
    inherits: iface.inherits.clone(),
    ext_attrs: iface.ext_attrs.clone(),
    exposure: iface.exposure.clone(),
    members: iface.members.iter().map(analyze_member).collect(),
  }
}

fn analyze_mixin(mixin: &ResolvedInterfaceMixin) -> AnalyzedInterfaceMixin {
  AnalyzedInterfaceMixin {
    name: mixin.name.clone(),
    ext_attrs: mixin.ext_attrs.clone(),
    members: mixin.members.iter().map(analyze_member).collect(),
  }
}

fn analyze_member(member: &ResolvedInterfaceMember) -> AnalyzedInterfaceMember {
  let parsed = match parse_interface_member(&member.raw) {
    Ok(m) => m,
    Err(_) => InterfaceMember::Unparsed {
      raw: member.raw.clone(),
    },
  };
  AnalyzedInterfaceMember {
    name: member.name.clone(),
    ext_attrs: member.ext_attrs.clone(),
    exposure: member.exposure.clone(),
    raw: member.raw.clone(),
    parsed,
  }
}

