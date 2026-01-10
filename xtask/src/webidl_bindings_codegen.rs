use anyhow::{bail, Context, Result};
use clap::{Args, ValueEnum};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use fastrender::webidl::{
  WebIdlCallback, WebIdlDictionary, WebIdlEnum, WebIdlExtendedAttribute, WebIdlInterface,
  WebIdlInterfaceMember, WebIdlInterfaceMixin, WebIdlTypedef, WebIdlWorld,
};

use crate::webidl::analyze::AnalyzedWebIdlWorld;
use crate::webidl::ExtendedAttribute;
use crate::webidl::ast::{Argument, BuiltinType, IdlLiteral, IdlType, InterfaceMember};
use crate::webidl::resolve::{ExposureTarget, ResolvedWebIdlWorld};
use crate::webidl::type_resolution;
use crate::webidl::type_resolution::{build_type_context, expand_typedefs_in_type};
use webidl_ir::{
  DefaultValue as IrDefaultValue, IdlType as IrIdlType, NamedTypeKind,
  NumericType as IrNumericType, TypeAnnotation as IrTypeAnnotation, TypeContext,
};

#[derive(Args, Debug)]
pub struct WebIdlBindingsCodegenArgs {
  /// Codegen backend.
  ///
  /// - `realm` (default): emit only the Window-facing WebIDL wrappers.
  /// - `legacy`: also emit the deprecated `VmJsRuntime` DOM scaffold (`src/js/legacy/dom_generated.rs`).
  #[arg(long, default_value = "realm", value_enum)]
  pub backend: WebIdlBindingsBackend,

  /// Output Rust module path (relative to repo root unless absolute).
  #[arg(
    long,
    default_value = "src/js/webidl/bindings/generated/mod.rs",
    value_name = "FILE"
  )]
  pub out: PathBuf,

  /// Path to the Window-facing WebIDL bindings allowlist manifest (TOML).
  #[arg(
    long,
    default_value = "tools/webidl/window_bindings_allowlist.toml",
    value_name = "FILE"
  )]
  pub window_allowlist: PathBuf,

  /// Path to the DOM-scaffold bindings allowlist manifest (TOML).
  ///
  /// Only used when `--backend legacy` is passed.
  #[arg(
    long,
    default_value = "tools/webidl/bindings_allowlist.toml",
    value_name = "FILE"
  )]
  pub dom_allowlist: PathBuf,

  /// Output Rust module path for the DOM-scaffold bindings (relative to repo root unless absolute).
  ///
  /// Only used when `--backend legacy` is passed.
  #[arg(
    long,
    default_value = "src/js/legacy/dom_generated.rs",
    value_name = "FILE"
  )]
  pub dom_out: PathBuf,

  /// Do not write files; instead, fail if the generated output differs.
  #[arg(long)]
  pub check: bool,

  /// Which WebIDL exposure target(s) to generate bindings for.
  #[arg(long, value_enum, default_value_t = ExposureTarget::All)]
  pub exposure_target: ExposureTarget,

  /// Interface allow-list override (can be passed multiple times).
  ///
  /// If supplied, this bypasses the committed Window bindings allowlist manifest and emits *all*
  /// constructors/operations for the selected interfaces (useful for local experiments).
  #[arg(long = "allow-interface", value_name = "NAME")]
  pub allow_interfaces: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum WebIdlBindingsBackend {
  /// Window-facing bindings only (vm-js realm DOM bindings are handwritten).
  Realm,
  /// Also generate the deprecated `VmJsRuntime` DOM scaffold.
  Legacy,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebIdlBindingsGenerationMode {
  /// Emit only allowlisted constructors/operations (driven by `window_bindings_allowlist.toml`).
  Allowlist,
  /// Emit operations/constructors for all selected interfaces (used by unit tests).
  AllMembers,
}

#[derive(Debug, Clone, Default)]
pub struct WebIdlInterfaceAllowlist {
  pub constructors: bool,
  pub attributes: BTreeSet<String>,
  pub operations: BTreeSet<String>,
}

#[derive(Debug, Clone)]
pub struct WebIdlBindingsCodegenConfig {
  pub mode: WebIdlBindingsGenerationMode,
  pub allow_interfaces: BTreeSet<String>,
  pub interface_allowlist: BTreeMap<String, WebIdlInterfaceAllowlist>,
  pub prototype_chains: bool,
}

pub fn run_webidl_bindings_codegen(args: WebIdlBindingsCodegenArgs) -> Result<()> {
  let repo_root = repo_root();
  let rustfmt_config = repo_root.join(".rustfmt.toml");

  let out_path = absolutize(repo_root.clone(), args.out);
  let window_allowlist_path = absolutize(repo_root.clone(), args.window_allowlist);
  let dom_allowlist_path = absolutize(repo_root.clone(), args.dom_allowlist);
  let dom_out_path = absolutize(repo_root.clone(), args.dom_out);

  // Prefer the committed snapshot (`src/webidl/generated`) so running this xtask does not require
  // vendored spec submodules.
  let snapshot_world: &WebIdlWorld = &fastrender::webidl::generated::WORLD;
  let snapshot_idl = snapshot_world_to_idl(snapshot_world);

  let window_config = if args.allow_interfaces.is_empty() {
    let allowlist_text = fs::read_to_string(&window_allowlist_path).with_context(|| {
      format!(
        "read WebIDL Window bindings allowlist {}",
        window_allowlist_path.display()
      )
    })?;
    let manifest: WindowBindingsAllowlistManifest =
      toml::from_str(&allowlist_text).context("parse WebIDL Window bindings allowlist TOML")?;
    let interface_allowlist =
      window_parse_allowlisted_interfaces(snapshot_world, &manifest.interfaces)?;
    WebIdlBindingsCodegenConfig {
      mode: WebIdlBindingsGenerationMode::Allowlist,
      allow_interfaces: interface_allowlist.keys().cloned().collect(),
      interface_allowlist,
      prototype_chains: manifest.prototype_chains,
    }
  } else {
    WebIdlBindingsCodegenConfig {
      mode: WebIdlBindingsGenerationMode::AllMembers,
      allow_interfaces: args.allow_interfaces.into_iter().collect(),
      interface_allowlist: BTreeMap::new(),
      prototype_chains: true,
    }
  };

  let generated_bindings = generate_bindings_module_from_idl_with_config(
    &snapshot_idl,
    &rustfmt_config,
    args.exposure_target,
    window_config,
  )
  .context("generate WebIDL bindings module")?;

  let generated_dom = if args.backend == WebIdlBindingsBackend::Legacy {
    let dom_allowlist_text = fs::read_to_string(&dom_allowlist_path).with_context(|| {
      format!(
        "read WebIDL DOM bindings allowlist {}",
        dom_allowlist_path.display()
      )
    })?;
    let dom_manifest: DomAllowlistManifest = toml::from_str(&dom_allowlist_text)
      .context("parse WebIDL DOM bindings allowlist TOML")?;
    Some(
      generate_dom_bindings_module(&dom_manifest, &rustfmt_config)
        .context("generate DOM scaffold bindings module")?,
    )
  } else {
    None
  };

  if args.check {
    let existing = fs::read_to_string(&out_path)
      .with_context(|| format!("read generated file {}", out_path.display()))?;
    if existing != generated_bindings {
      bail!(
        "generated WebIDL bindings are out of date: run `bash scripts/cargo_agent.sh xtask webidl-bindings` (path={})",
        out_path.display()
      );
    }

    if let Some(generated_dom) = generated_dom.as_ref() {
      let existing_dom = fs::read_to_string(&dom_out_path)
        .with_context(|| format!("read generated file {}", dom_out_path.display()))?;
      if existing_dom != *generated_dom {
        bail!(
          "generated DOM bindings are out of date: run `bash scripts/cargo_agent.sh xtask webidl-bindings --backend legacy` (path={})",
          dom_out_path.display()
        );
      }
    }
    return Ok(());
  }

  if let Some(parent) = out_path.parent() {
    fs::create_dir_all(parent)
      .with_context(|| format!("create output directory {}", parent.display()))?;
  }
  fs::write(&out_path, generated_bindings)
    .with_context(|| format!("write generated output {}", out_path.display()))?;

  if let Some(generated_dom) = generated_dom {
    if let Some(parent) = dom_out_path.parent() {
      fs::create_dir_all(parent)
        .with_context(|| format!("create output directory {}", parent.display()))?;
    }
    fs::write(&dom_out_path, generated_dom)
      .with_context(|| format!("write generated output {}", dom_out_path.display()))?;
  }

  Ok(())
}

pub fn generate_bindings_module_from_idl_with_config(
  idl: &str,
  rustfmt_config_path: &Path,
  exposure_target: ExposureTarget,
  config: WebIdlBindingsCodegenConfig,
) -> Result<String> {
  let parsed = crate::webidl::parse_webidl(idl).context("parse WebIDL")?;
  let resolved = crate::webidl::resolve::resolve_webidl_world(&parsed);
  let raw = generate_bindings_module_unformatted(&resolved, exposure_target, &config)
    .context("generate WebIDL bindings module (unformatted)")?;
  let formatted = crate::webidl::generate::rustfmt(&raw, rustfmt_config_path)?;
  crate::webidl::generate::ensure_no_forbidden_tokens(&formatted)?;
  Ok(formatted)
}

fn absolutize(repo_root: PathBuf, path: PathBuf) -> PathBuf {
  if path.is_absolute() {
    path
  } else {
    repo_root.join(path)
  }
}

fn repo_root() -> PathBuf {
  let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
  manifest_dir
    .parent()
    .map(|p| p.to_path_buf())
    .unwrap_or_else(|| manifest_dir.to_path_buf())
}

fn snapshot_world_to_idl(world: &WebIdlWorld) -> String {
  let mut out = String::new();

  for en in world.enums {
    write_enum_to_idl(&mut out, en);
    out.push('\n');
  }
  for td in world.typedefs {
    write_typedef_to_idl(&mut out, td);
    out.push('\n');
  }
  for cb in world.callbacks {
    write_callback_to_idl(&mut out, cb);
    out.push('\n');
  }
  for dict in world.dictionaries {
    write_dictionary_to_idl(&mut out, dict);
    out.push('\n');
  }
  for mixin in world.interface_mixins {
    write_interface_mixin_to_idl(&mut out, mixin);
    out.push('\n');
  }
  for iface in world.interfaces {
    write_interface_to_idl(&mut out, iface);
    out.push('\n');
  }

  out
}

fn write_ext_attrs_to_idl(out: &mut String, indent: &str, attrs: &[WebIdlExtendedAttribute]) {
  if attrs.is_empty() {
    return;
  }

  out.push_str(indent);
  out.push('[');
  for (idx, attr) in attrs.iter().enumerate() {
    if idx != 0 {
      out.push_str(", ");
    }
    out.push_str(attr.name);
    if let Some(value) = attr.value {
      out.push('=');
      out.push_str(value);
    }
  }
  out.push_str("]\n");
}

fn write_interface_member_to_idl(out: &mut String, member: &WebIdlInterfaceMember) {
  write_ext_attrs_to_idl(out, "  ", member.ext_attrs);
  out.push_str("  ");
  out.push_str(member.raw);
  out.push_str(";\n");
}

fn write_interface_to_idl(out: &mut String, iface: &WebIdlInterface) {
  write_ext_attrs_to_idl(out, "", iface.ext_attrs);
  out.push_str(if iface.callback {
    "callback interface "
  } else {
    "interface "
  });
  out.push_str(iface.name);
  if let Some(parent) = iface.inherits {
    out.push_str(" : ");
    out.push_str(parent);
  }
  out.push_str(" {\n");
  for member in iface.members {
    write_interface_member_to_idl(out, member);
  }
  out.push_str("};\n");
}

fn write_interface_mixin_to_idl(out: &mut String, mixin: &WebIdlInterfaceMixin) {
  write_ext_attrs_to_idl(out, "", mixin.ext_attrs);
  out.push_str("interface mixin ");
  out.push_str(mixin.name);
  out.push_str(" {\n");
  for member in mixin.members {
    write_interface_member_to_idl(out, member);
  }
  out.push_str("};\n");
}

fn write_dictionary_to_idl(out: &mut String, dict: &WebIdlDictionary) {
  write_ext_attrs_to_idl(out, "", dict.ext_attrs);
  out.push_str("dictionary ");
  out.push_str(dict.name);
  if let Some(parent) = dict.inherits {
    out.push_str(" : ");
    out.push_str(parent);
  }
  out.push_str(" {\n");
  for member in dict.members {
    write_ext_attrs_to_idl(out, "  ", member.ext_attrs);
    out.push_str("  ");
    out.push_str(member.raw);
    out.push_str(";\n");
  }
  out.push_str("};\n");
}

fn write_enum_to_idl(out: &mut String, en: &WebIdlEnum) {
  write_ext_attrs_to_idl(out, "", en.ext_attrs);
  out.push_str("enum ");
  out.push_str(en.name);
  out.push_str(" {\n");
  for (idx, value) in en.values.iter().enumerate() {
    out.push_str("  ");
    out.push_str(&idl_string_literal(value));
    if idx + 1 != en.values.len() {
      out.push(',');
    }
    out.push('\n');
  }
  out.push_str("};\n");
}

fn write_typedef_to_idl(out: &mut String, td: &WebIdlTypedef) {
  write_ext_attrs_to_idl(out, "", td.ext_attrs);
  out.push_str("typedef ");
  out.push_str(td.type_);
  out.push(' ');
  out.push_str(td.name);
  out.push_str(";\n");
}

fn write_callback_to_idl(out: &mut String, cb: &WebIdlCallback) {
  write_ext_attrs_to_idl(out, "", cb.ext_attrs);
  out.push_str("callback ");
  out.push_str(cb.name);
  out.push_str(" = ");
  out.push_str(cb.type_);
  out.push_str(";\n");
}

fn idl_string_literal(value: &str) -> String {
  let mut out = String::with_capacity(value.len() + 2);
  out.push('"');
  for ch in value.chars() {
    match ch {
      '"' => out.push_str("\\\""),
      '\\' => out.push_str("\\\\"),
      '\n' => out.push_str("\\n"),
      '\r' => out.push_str("\\r"),
      '\t' => out.push_str("\\t"),
      _ => out.push(ch),
    }
  }
  out.push('"');
  out
}

fn default_true() -> bool {
  true
}

#[derive(Debug, Deserialize)]
struct WindowBindingsAllowlistManifest {
  #[serde(default = "default_true")]
  prototype_chains: bool,
  #[serde(rename = "interface")]
  interfaces: Vec<WindowBindingsAllowlistInterface>,
}

#[derive(Debug, Deserialize)]
struct WindowBindingsAllowlistInterface {
  name: String,
  #[serde(default)]
  constructors: bool,
  #[serde(default)]
  attributes: Vec<String>,
  #[serde(default)]
  operations: Vec<String>,
}

fn window_parse_allowlisted_interfaces(
  world: &WebIdlWorld,
  allowlist: &[WindowBindingsAllowlistInterface],
) -> Result<BTreeMap<String, WebIdlInterfaceAllowlist>> {
  let mut out = BTreeMap::new();
  let mut seen = BTreeSet::new();

  for entry in allowlist {
    if !seen.insert(entry.name.clone()) {
      bail!(
        "Window bindings allowlist contains duplicate interface entry: {}",
        entry.name
      );
    }

    let iface = world.interface(&entry.name).with_context(|| {
      format!(
        "allowlisted interface `{}` is missing from WORLD",
        entry.name
      )
    })?;

    out.insert(entry.name.clone(), window_parse_interface_entry(iface, entry)?);
  }

  Ok(out)
}

fn window_parse_interface_entry(
  iface: &WebIdlInterface,
  allow: &WindowBindingsAllowlistInterface,
) -> Result<WebIdlInterfaceAllowlist> {
  // Constructors.
  if allow.constructors {
    let mut found_ctor = false;
    for member in iface.members {
      if member.name != Some("constructor") {
        continue;
      }
      let parsed = crate::webidl::parse_interface_member(member.raw).with_context(|| {
        format!(
          "failed to parse WebIDL member `{}` constructor `{}`",
          iface.name, member.raw
        )
      })?;
      if matches!(parsed, InterfaceMember::Constructor { .. }) {
        found_ctor = true;
      }
    }
    if !found_ctor {
      bail!(
        "Window bindings allowlist requested constructors for `{}`, but none were found in WORLD",
        iface.name
      );
    }
  }

  // Attributes.
  let mut attributes: BTreeSet<String> = BTreeSet::new();
  for attr_name in &allow.attributes {
    if !attributes.insert(attr_name.clone()) {
      bail!(
        "Window bindings allowlist contains duplicate attribute `{}` on interface `{}`",
        attr_name,
        iface.name
      );
    }

    let mut matches = Vec::new();
    for member in iface.members {
      if member.name != Some(attr_name.as_str()) {
        continue;
      }
      let parsed = crate::webidl::parse_interface_member(member.raw).with_context(|| {
        format!(
          "failed to parse WebIDL member `{}` attribute `{}`",
          iface.name, member.raw
        )
      })?;
      if matches!(parsed, InterfaceMember::Attribute { .. }) {
        matches.push(parsed);
      }
    }
    if matches.is_empty() {
      bail!(
        "allowlisted attribute `{}` was not found on interface `{}`",
        attr_name,
        iface.name
      );
    }
    if matches.len() != 1 {
      bail!(
        "allowlisted attribute `{}` appears multiple times on interface `{}`; overloads are not supported for attributes",
        attr_name,
        iface.name
      );
    }
  }

  // Operations.
  let mut operations: BTreeSet<String> = BTreeSet::new();
  for op_name in &allow.operations {
    if !operations.insert(op_name.clone()) {
      bail!(
        "Window bindings allowlist contains duplicate operation `{}` on interface `{}`",
        op_name,
        iface.name
      );
    }

    let mut found = false;
    for member in iface.members {
      if member.name != Some(op_name.as_str()) {
        continue;
      }
      let parsed = crate::webidl::parse_interface_member(member.raw).with_context(|| {
        format!(
          "failed to parse WebIDL member `{}` operation `{}`",
          iface.name, member.raw
        )
      })?;
      let InterfaceMember::Operation {
        name: Some(name),
        special: None,
        ..
      } = &parsed
      else {
        continue;
      };
      if name == op_name {
        found = true;
      }
    }
    if !found {
      bail!(
        "allowlisted operation `{}` was not found on interface `{}`",
        op_name,
        iface.name
      );
    }
  }

  Ok(WebIdlInterfaceAllowlist {
    constructors: allow.constructors,
    attributes,
    operations,
  })
}

#[derive(Debug, Deserialize)]
struct DomAllowlistManifest {
  #[serde(rename = "interface")]
  interfaces: Vec<DomAllowlistInterface>,
}

#[derive(Debug, Deserialize)]
struct DomAllowlistInterface {
  name: String,
  #[serde(default)]
  constructors: bool,
  #[serde(default)]
  attributes: Vec<String>,
  #[serde(default)]
  operations: Vec<String>,
}

#[derive(Debug, Clone)]
struct DomParsedInterface {
  name: String,
  inherits: Option<String>,
  constructible: bool,
  attributes: Vec<InterfaceMember>,
  operations: Vec<InterfaceMember>,
}

fn generate_dom_bindings_module(
  manifest: &DomAllowlistManifest,
  rustfmt_config_path: &Path,
) -> Result<String> {
  let world = &fastrender::webidl::generated::WORLD;

  let allowlisted = dom_parse_allowlisted_interfaces(world, &manifest.interfaces)?;
  let derived_map = dom_compute_derived_interfaces(&allowlisted);

  let raw = dom_render_bindings_module(&allowlisted, &derived_map)?;
  let formatted = crate::webidl::generate::rustfmt(&raw, rustfmt_config_path)?;
  crate::webidl::generate::ensure_no_forbidden_tokens(&formatted)?;
  Ok(formatted)
}

fn dom_parse_allowlisted_interfaces(
  world: &WebIdlWorld,
  allowlist: &[DomAllowlistInterface],
) -> Result<Vec<DomParsedInterface>> {
  let mut out = Vec::new();
  let mut seen = BTreeSet::new();

  for entry in allowlist {
    if !seen.insert(entry.name.clone()) {
      bail!(
        "DOM allowlist contains duplicate interface entry: {}",
        entry.name
      );
    }
    let iface = world.interface(&entry.name).with_context(|| {
      format!(
        "allowlisted interface `{}` is missing from WORLD",
        entry.name
      )
    })?;
    out.push(dom_parse_interface_entry(iface, entry)?);
  }

  Ok(out)
}

fn dom_parse_interface_entry(
  iface: &WebIdlInterface,
  allow: &DomAllowlistInterface,
) -> Result<DomParsedInterface> {
  let mut constructible = false;
  let mut attributes: Vec<InterfaceMember> = Vec::new();
  let mut operations: Vec<InterfaceMember> = Vec::new();

  // Constructors.
  if allow.constructors {
    for member in iface.members {
      if member.name != Some("constructor") {
        continue;
      }
      let parsed = crate::webidl::parse_interface_member(member.raw).with_context(|| {
        format!(
          "failed to parse WebIDL member `{}` constructor `{}`",
          iface.name, member.raw
        )
      })?;
      let InterfaceMember::Constructor { arguments } = parsed else {
        continue;
      };
      if !arguments.is_empty() {
        bail!(
          "only no-argument constructors are supported in MVP DOM bindings (interface={})",
          iface.name
        );
      }
      constructible = true;
    }
    if !constructible {
      bail!(
        "DOM allowlist requested constructors for `{}`, but none were found in WORLD",
        iface.name
      );
    }
  }

  // Attributes.
  for attr_name in &allow.attributes {
    let mut matches = Vec::new();
    for member in iface.members {
      if member.name != Some(attr_name.as_str()) {
        continue;
      }
      let parsed = crate::webidl::parse_interface_member(member.raw).with_context(|| {
        format!(
          "failed to parse WebIDL member `{}` attribute `{}`",
          iface.name, member.raw
        )
      })?;
      if matches!(parsed, InterfaceMember::Attribute { .. }) {
        matches.push(parsed);
      }
    }
    if matches.is_empty() {
      bail!(
        "allowlisted attribute `{}` was not found on interface `{}`",
        attr_name,
        iface.name
      );
    }
    if matches.len() != 1 {
      bail!(
        "allowlisted attribute `{}` appears multiple times on interface `{}`; overloads are not supported for attributes",
        attr_name,
        iface.name
      );
    }
    attributes.push(matches.remove(0));
  }

  // Operations.
  for op_name in &allow.operations {
    let mut matches = Vec::new();
    for member in iface.members {
      if member.name != Some(op_name.as_str()) {
        continue;
      }
      let parsed = crate::webidl::parse_interface_member(member.raw).with_context(|| {
        format!(
          "failed to parse WebIDL member `{}` operation `{}`",
          iface.name, member.raw
        )
      })?;
      let InterfaceMember::Operation {
        name: Some(name),
        static_,
        special: None,
        ..
      } = &parsed
      else {
        continue;
      };
      if name != op_name {
        continue;
      }
      if *static_ {
        bail!(
          "static operations are not supported in MVP DOM bindings (interface={}, operation={})",
          iface.name,
          op_name
        );
      }
      matches.push(parsed);
    }
    if matches.is_empty() {
      bail!(
        "allowlisted operation `{}` was not found on interface `{}`",
        op_name,
        iface.name
      );
    }
    operations.extend(matches);
  }

  Ok(DomParsedInterface {
    name: iface.name.to_string(),
    inherits: iface.inherits.map(|s| s.to_string()),
    constructible,
    attributes,
    operations,
  })
}

fn dom_compute_derived_interfaces(
  interfaces: &[DomParsedInterface],
) -> BTreeMap<String, BTreeSet<String>> {
  let mut by_name: BTreeMap<String, &DomParsedInterface> = BTreeMap::new();
  for iface in interfaces {
    by_name.insert(iface.name.clone(), iface);
  }

  let mut derived: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
  for iface in interfaces {
    // Every interface is derived from itself.
    derived
      .entry(iface.name.clone())
      .or_default()
      .insert(iface.name.clone());

    let mut cur = iface.inherits.clone();
    while let Some(parent) = cur {
      derived
        .entry(parent.clone())
        .or_default()
        .insert(iface.name.clone());

      cur = by_name.get(&parent).and_then(|i| i.inherits.clone());
    }
  }
  derived
}

fn dom_render_bindings_module(
  interfaces: &[DomParsedInterface],
  derived: &BTreeMap<String, BTreeSet<String>>,
) -> Result<String> {
  let mut out = String::new();

  out.push_str("// @generated by `bash scripts/cargo_agent.sh xtask webidl-bindings`. DO NOT EDIT.\n");
  out.push_str("//\n");
  out.push_str("// Source inputs:\n");
  out.push_str(
    "// - src/webidl/generated/mod.rs (committed snapshot; produced by `bash scripts/cargo_agent.sh xtask webidl`)\n",
  );
  out.push_str("// - tools/webidl/bindings_allowlist.toml\n\n");
  out.push_str("use super::DomHost;\n");
  out.push_str("use webidl_js_runtime::{JsRuntime, VmJsRuntime, WebIdlJsRuntime};\n");
  out.push_str("use vm_js::{PropertyKey, Value, VmError};\n\n");

  out.push_str(
    "pub fn install_dom_bindings(rt: &mut VmJsRuntime, host: &mut impl DomHost) -> Result<(), VmError> {\n",
  );

  // Common property keys.
  out.push_str("  let k_dom_type: PropertyKey = rt.prop_key(\"__fastrender_dom_type\")?;\n");
  out.push_str("  let k_prototype: PropertyKey = rt.prop_key(\"prototype\")?;\n");
  out.push_str("  let k_constructor: PropertyKey = rt.prop_key(\"constructor\")?;\n\n");

  // Pre-intern all interface name tags (used for this-brand checks).
  for iface in interfaces {
    let var = format!("tag_{}", dom_to_snake(&iface.name));
    out.push_str(&format!(
      "  let {var}: Value = rt.alloc_string_value(\"{}\")?;\n",
      iface.name
    ));
  }
  out.push('\n');

  // Prototypes and constructors.
  for iface in interfaces {
    let snake = dom_to_snake(&iface.name);
    out.push_str(&format!(
      "  let proto_{snake}: Value = rt.alloc_object_value()?;\n"
    ));

    // Create the interface object (constructor function).
    out.push_str(&format!(
      "  let ctor_{snake}: Value = rt.alloc_function_value({{\n"
    ));

    if !iface.constructible {
      out.push_str(
        "    move |rt, _this, _args| Err(rt.throw_type_error(\"Illegal constructor\"))\n",
      );
    } else {
      let tag_var = format!("tag_{snake}");
      out.push_str(&format!(
        "    let proto: Value = proto_{snake};\n    let tag: Value = {tag_var};\n    let k_dom_type: PropertyKey = k_dom_type;\n    move |rt, _this, _args| {{\n      let obj: Value = rt.alloc_object_value()?;\n      rt.set_prototype(obj, Some(proto))?;\n      rt.define_data_property(obj, k_dom_type, tag, false)?;\n      Ok(obj)\n    }}\n"
      ));
    }

    out.push_str("  })?;\n");

    // ctor.prototype = proto (non-enumerable)
    out.push_str(&format!(
      "  rt.define_data_property(ctor_{snake}, k_prototype, proto_{snake}, false)?;\n"
    ));
    // proto.constructor = ctor (non-enumerable)
    out.push_str(&format!(
      "  rt.define_data_property(proto_{snake}, k_constructor, ctor_{snake}, false)?;\n\n"
    ));
  }

  // Prototype inheritance.
  for iface in interfaces {
    let Some(parent) = &iface.inherits else {
      continue;
    };
    let parent_snake = dom_to_snake(parent);
    let child_snake = dom_to_snake(&iface.name);
    out.push_str(&format!(
      "  rt.set_prototype(proto_{child_snake}, Some(proto_{parent_snake}))?;\n"
    ));
  }
  out.push('\n');

  // Define members on prototypes.
  for iface in interfaces {
    dom_render_interface_members(&mut out, iface, derived)?;
  }

  // Expose constructors on the global object.
  out.push_str("  let global: Value = host.global_object();\n");
  for iface in interfaces {
    let snake = dom_to_snake(&iface.name);
    let key_var = format!("k_global_iface_{}", snake);
    out.push_str(&format!(
      "  let {key_var}: PropertyKey = rt.prop_key(\"{}\")?;\n",
      iface.name
    ));
    out.push_str(&format!(
      "  rt.define_data_property(global, {key_var}, ctor_{snake}, false)?;\n",
    ));
  }

  // Minimal bootstrapping globals for unit tests / early integration:
  // - Brand the global object as `Window`
  // - Install a `document` object.
  if interfaces.iter().any(|i| i.name == "Window") {
    out.push_str("  rt.set_prototype(global, Some(proto_window))?;\n");
    out.push_str("  rt.define_data_property(global, k_dom_type, tag_window, false)?;\n");
  }
  if interfaces.iter().any(|i| i.name == "Document") {
    out.push_str("  let document: Value = rt.alloc_object_value()?;\n");
    out.push_str("  rt.set_prototype(document, Some(proto_document))?;\n");
    out.push_str("  rt.define_data_property(document, k_dom_type, tag_document, false)?;\n");
    out.push_str("  let k_global_document: PropertyKey = rt.prop_key(\"document\")?;\n");
    out.push_str("  rt.define_data_property(global, k_global_document, document, false)?;\n");
  }

  out.push_str("  Ok(())\n}\n");

  Ok(out)
}

fn dom_render_interface_members(
  out: &mut String,
  iface: &DomParsedInterface,
  derived: &BTreeMap<String, BTreeSet<String>>,
) -> Result<()> {
  let iface_snake = dom_to_snake(&iface.name);

  // Attributes.
  for member in &iface.attributes {
    let InterfaceMember::Attribute { name, readonly, .. } = member else {
      continue;
    };

    let key_var = format!("k_{}_{}", iface_snake, dom_to_snake(name));
    out.push_str(&format!(
      "  let {key_var}: PropertyKey = rt.prop_key(\"{name}\")?;\n"
    ));

    let getter_var = format!("get_{}_{}", iface_snake, dom_to_snake(name));
    let allowed = derived
      .get(&iface.name)
      .cloned()
      .unwrap_or_else(BTreeSet::new);
    let allowed_cond = dom_render_allowed_checks("this_type", &allowed);

    out.push_str(&format!(
      "  let {getter_var}: Value = rt.alloc_function_value({{\n    let k_dom_type: PropertyKey = k_dom_type;\n{allowed_checks}    move |rt, this, _args| {{\n      if !rt.is_object(this) {{\n        return Err(rt.throw_type_error(\"Illegal invocation\"));\n      }}\n      let this_type: Value = rt.get(this, k_dom_type)?;\n      if !({allowed_checks_cond}) {{\n        return Err(rt.throw_type_error(\"Illegal invocation\"));\n      }}\n      Ok(Value::Undefined)\n    }}\n  }})?;\n",
      getter_var = getter_var,
      allowed_checks = dom_render_allowed_captures(&allowed),
      allowed_checks_cond = allowed_cond.clone(),
    ));

    let setter_expr = if *readonly {
      "Value::Undefined".to_string()
    } else {
      // Non-readonly attribute: generate a stub setter.
      let setter_var = format!("set_{}_{}", iface_snake, dom_to_snake(name));
      out.push_str(&format!(
        "  let {setter_var}: Value = rt.alloc_function_value({{\n    let k_dom_type: PropertyKey = k_dom_type;\n{captures}    move |rt, this, _args| {{\n      if !rt.is_object(this) {{\n        return Err(rt.throw_type_error(\"Illegal invocation\"));\n      }}\n      let this_type: Value = rt.get(this, k_dom_type)?;\n      if !({cond}) {{\n        return Err(rt.throw_type_error(\"Illegal invocation\"));\n      }}\n      Err(rt.throw_type_error(\"not implemented\"))\n    }}\n  }})?;\n",
        setter_var = setter_var,
        captures = dom_render_allowed_captures(&allowed),
        cond = allowed_cond.clone(),
      ));
      setter_var
    };

    out.push_str(&format!(
      "  rt.define_accessor_property(proto_{iface_snake}, {key_var}, {getter_var}, {setter_expr}, false)?;\n",
      iface_snake = iface_snake,
      key_var = key_var,
      getter_var = getter_var,
      setter_expr = setter_expr
    ));
  }

  // Operations.
  for member in &iface.operations {
    let InterfaceMember::Operation {
      name: Some(name),
      arguments,
      ..
    } = member
    else {
      continue;
    };

    let key_var = format!("k_{}_{}", iface_snake, dom_to_snake(name));
    out.push_str(&format!(
      "  let {key_var}: PropertyKey = rt.prop_key(\"{name}\")?;\n"
    ));

    let func_var = format!("fn_{}_{}", iface_snake, dom_to_snake(name));
    let allowed = derived
      .get(&iface.name)
      .cloned()
      .unwrap_or_else(BTreeSet::new);
    let allowed_cond = dom_render_allowed_checks("this_type", &allowed);

    let min_required = arguments
      .iter()
      .take_while(|a| !a.optional && !a.variadic)
      .count();

    let required_args_check = if min_required == 0 {
      String::new()
    } else {
      format!(
        "      if args.len() < {min_required} {{\n        return Err(rt.throw_type_error(&format!(\"{iface}.{name}: expected at least {min_required} arguments, got {{}}\", args.len())));\n      }}\n",
        min_required = min_required,
        iface = iface.name,
        name = name
      )
    };

    // Special-case a common DOM pattern: `optional (Dictionary or boolean) options = {}`
    // We treat it as a two-overload set for MVP dispatch.
    let overload_dispatch = dom_render_union_boolean_dictionary_dispatch(iface, name, arguments)?;

    out.push_str(&format!(
      "  let {func_var}: Value = rt.alloc_function_value({{\n    let k_dom_type: PropertyKey = k_dom_type;\n{captures}    move |rt, this, args| {{\n      if !rt.is_object(this) {{\n        return Err(rt.throw_type_error(\"Illegal invocation\"));\n      }}\n      let this_type: Value = rt.get(this, k_dom_type)?;\n      if !({cond}) {{\n        return Err(rt.throw_type_error(\"Illegal invocation\"));\n      }}\n{required_args_check}{overload_dispatch}      Err(rt.throw_type_error(\"not implemented\"))\n    }}\n  }})?;\n",
      func_var = func_var,
      captures = dom_render_allowed_captures(&allowed),
      cond = allowed_cond,
      required_args_check = required_args_check,
      overload_dispatch = overload_dispatch
    ));

    out.push_str(&format!(
      "  rt.define_data_property(proto_{iface_snake}, {key_var}, {func_var}, false)?;\n",
      iface_snake = iface_snake,
      key_var = key_var,
      func_var = func_var
    ));
  }

  if !iface.attributes.is_empty() || !iface.operations.is_empty() {
    out.push('\n');
  }

  Ok(())
}

fn dom_render_allowed_captures(allowed: &BTreeSet<String>) -> String {
  let mut out = String::new();
  for name in allowed {
    out.push_str(&format!(
      "    let tag_{snake}: Value = tag_{snake};\n",
      snake = dom_to_snake(name)
    ));
  }
  out
}

fn dom_render_allowed_checks(var: &str, allowed: &BTreeSet<String>) -> String {
  if allowed.is_empty() {
    return "false".to_string();
  }
  let mut cond = String::new();
  for (idx, name) in allowed.iter().enumerate() {
    if idx != 0 {
      cond.push_str(" || ");
    }
    cond.push_str(&format!("{var} == tag_{}", dom_to_snake(name)));
  }
  cond
}

fn dom_render_union_boolean_dictionary_dispatch(
  iface: &DomParsedInterface,
  op_name: &str,
  args: &[Argument],
) -> Result<String> {
  let Some(last) = args.last() else {
    return Ok(String::new());
  };
  if !last.optional {
    return Ok(String::new());
  }

  let IdlType::Union(members) = &last.type_ else {
    return Ok(String::new());
  };
  if members.len() != 2 {
    return Ok(String::new());
  }

  let (a, b) = (&members[0], &members[1]);

  let (dict_name, bool_first) = match (a, b) {
    (IdlType::Named(name), IdlType::Builtin(BuiltinType::Boolean)) => (name.as_str(), false),
    (IdlType::Builtin(BuiltinType::Boolean), IdlType::Named(name)) => (name.as_str(), true),
    _ => return Ok(String::new()),
  };

  // Only support dictionary-or-boolean unions for MVP dispatch. We consider any non-boolean value
  // to take the dictionary path.
  let _ = dict_name;
  let _ = bool_first;

  // Validate as an overload set using the existing overload-set algorithms.
  dom_validate_boolean_dictionary_overload_set(iface, op_name)?;

  Ok(
    "      // Overload-style dispatch for `optional (Dictionary or boolean)`.\n\
     \t// When no third argument is provided, follow the IDL union member order and take the\n\
     \t// dictionary branch.\n\
      if args.len() >= 3 {\n\
        let opt: Value = args[2];\n\
        if rt.is_boolean(opt) {\n\
          // boolean overload\n\
        } else {\n\
          // dictionary overload\n\
        }\n\
      }\n"
      .replace('\t', "  "),
  )
}

fn dom_validate_boolean_dictionary_overload_set(
  iface: &DomParsedInterface,
  op_name: &str,
) -> Result<()> {
  use crate::webidl::overload_ir::{
    validate_overload_set, Optionality, Overload, OverloadArgument, WorldContext,
  };
  use webidl_ir::{IdlType, NamedType, NamedTypeKind, StringType};

  struct SnapshotCtx<'a> {
    by_name: BTreeMap<&'a str, &'a str>,
  }

  impl<'a> WorldContext for SnapshotCtx<'a> {
    fn interface_inherits(&self, interface: &str) -> Option<&str> {
      self.by_name.get(interface).copied()
    }
  }

  // This validation currently only needs the local allowlisted inheritance chain.
  let mut by_name = BTreeMap::new();
  if let Some(parent) = iface.inherits.as_deref() {
    by_name.insert(iface.name.as_str(), parent);
  }

  let ctx = SnapshotCtx { by_name };

  // Minimal overload set: we only validate that the distinguishability algorithm accepts the
  // boolean-vs-dictionary branch.
  let overloads = vec![
    Overload {
      name: op_name.to_string(),
      arguments: vec![
        OverloadArgument::required(IdlType::String(StringType::DomString)),
        OverloadArgument {
          name: None,
          ty: IdlType::Named(NamedType {
            name: "EventListener".to_string(),
            kind: NamedTypeKind::Interface,
          }),
          optionality: Optionality::Required,
          default: None,
        },
        OverloadArgument {
          name: None,
          ty: IdlType::Named(NamedType {
            name: "Options".to_string(),
            kind: NamedTypeKind::Dictionary,
          }),
          optionality: Optionality::Optional,
          default: None,
        },
      ],
      origin: None,
    },
    Overload {
      name: op_name.to_string(),
      arguments: vec![
        OverloadArgument::required(IdlType::String(StringType::DomString)),
        OverloadArgument {
          name: None,
          ty: IdlType::Named(NamedType {
            name: "EventListener".to_string(),
            kind: NamedTypeKind::Interface,
          }),
          optionality: Optionality::Required,
          default: None,
        },
        OverloadArgument {
          name: None,
          ty: IdlType::Boolean,
          // The real DOM IDL models this as a union with a dictionary default (`options = {}`),
          // which means the boolean branch is only relevant when the third argument is provided.
          // Model it as required here so overload validation doesn't consider the ambiguous
          // two-argument form.
          optionality: Optionality::Required,
          default: None,
        },
      ],
      origin: None,
    },
  ];

  if let Err(diags) = validate_overload_set(&overloads, &ctx) {
    let mut msg = String::new();
    for diag in diags {
      if !msg.is_empty() {
        msg.push('\n');
      }
      msg.push_str(&diag.message);
    }
    bail!(
      "WebIDL overload validation failed for {}.{}:\n{}",
      iface.name,
      op_name,
      msg
    );
  }
  Ok(())
}

fn dom_to_snake(name: &str) -> String {
  let mut out = String::new();
  let mut prev_lower_or_digit = false;
  for ch in name.chars() {
    if ch.is_ascii_alphanumeric() {
      let is_upper = ch.is_ascii_uppercase();
      if is_upper && prev_lower_or_digit {
        out.push('_');
      }
      out.push(ch.to_ascii_lowercase());
      prev_lower_or_digit = ch.is_ascii_lowercase() || ch.is_ascii_digit();
      continue;
    }
    prev_lower_or_digit = false;
  }
  if out.is_empty() {
    "x".to_string()
  } else {
    out
  }
}

#[derive(Debug, Clone)]
struct SelectedInterface {
  name: String,
  inherits: Option<String>,
  constructors: Vec<ArgumentList>,
  operations: BTreeMap<String, Vec<OperationSig>>,
  static_operations: BTreeMap<String, Vec<OperationSig>>,
  iterable: Option<IterableInfo>,
  attributes: BTreeMap<String, AttributeSig>,
  static_attributes: BTreeMap<String, AttributeSig>,
  constants: BTreeMap<String, ConstantSig>,
}

#[derive(Debug, Clone)]
struct IterableInfo {
  async_: bool,
  key_type: Option<IrIdlType>,
  value_type: IrIdlType,
}

#[derive(Debug, Clone)]
struct OperationSig {
  raw: String,
  name: String,
  return_type: IdlType,
  arguments: Vec<Argument>,
}

#[derive(Debug, Clone)]
struct ArgumentList {
  raw: String,
  arguments: Vec<Argument>,
}

#[derive(Debug, Clone)]
struct AttributeSig {
  name: String,
  type_: IdlType,
  readonly: bool,
}

#[derive(Debug, Clone)]
struct ConstantSig {
  name: String,
  type_: IdlType,
  value: IdlLiteral,
}

fn generate_bindings_module_unformatted(
  resolved: &ResolvedWebIdlWorld,
  exposure_target: ExposureTarget,
  config: &WebIdlBindingsCodegenConfig,
) -> Result<String> {
  let mut out = String::new();

  out.push_str("// @generated by `bash scripts/cargo_agent.sh xtask webidl-bindings`. DO NOT EDIT.\n");
  out.push_str("//\n");
  out.push_str("// Source inputs:\n");
  out.push_str(
    "// - src/webidl/generated/mod.rs (committed snapshot; produced by `bash scripts/cargo_agent.sh xtask webidl`)\n",
  );
  out.push_str("\n");
  out.push_str("use super::host::{BindingValue, WebHostBindings};\n\n");

  let targets: &[ExposureTarget] = match exposure_target {
    ExposureTarget::All => &[ExposureTarget::Window, ExposureTarget::Worker],
    ExposureTarget::Window => &[ExposureTarget::Window],
    ExposureTarget::Worker => &[ExposureTarget::Worker],
  };

  let mut reexports: Vec<(String, String)> = Vec::new();

  for target in targets {
    let (module_name, install_fn_name, globals): (&str, &str, &[&str]) = match target {
      ExposureTarget::All => unreachable!(),
      ExposureTarget::Window => ("window", "install_window_bindings", &["Window"]),
      ExposureTarget::Worker => ("worker", "install_worker_bindings", &["WorkerGlobalScope"]),
    };

    let filtered = resolved.filter_by_exposure(*target);
    let analyzed = crate::webidl::analyze::analyze_resolved_world(&filtered);
    let type_ctx = build_type_context(&filtered).context("build WebIDL type context")?;
    let inner = generate_bindings_module_for_target_unformatted(
      &filtered,
      &analyzed,
      &type_ctx,
      config,
      install_fn_name,
      globals,
    )
    .with_context(|| format!("generate WebIDL bindings module for {module_name}"))?;

    out.push_str(&format!("pub mod {module_name} {{\n"));
    out.push_str(&indent_lines(&inner, 2));
    out.push_str("}\n\n");

    reexports.push((module_name.to_string(), install_fn_name.to_string()));
  }

  for (module_name, install_fn_name) in reexports {
    out.push_str(&format!("pub use {module_name}::{install_fn_name};\n"));
  }

  Ok(out)
}

fn generate_bindings_module_for_target_unformatted(
  resolved: &ResolvedWebIdlWorld,
  analyzed: &AnalyzedWebIdlWorld,
  type_ctx: &TypeContext,
  config: &WebIdlBindingsCodegenConfig,
  install_fn_name: &str,
  global_interfaces: &[&str],
) -> Result<String> {
  let is_global_iface = |name: &str| global_interfaces.iter().any(|g| *g == name);

  let selected = select_interfaces(resolved, analyzed, config)?;
  let referenced_dicts = collect_referenced_dictionaries(resolved, type_ctx, &selected);

  let mut out = String::new();

  out.push_str("use std::collections::BTreeMap;\n\n");
  out.push_str("use super::{BindingValue, WebHostBindings};\n\n");
  out.push_str("use crate::js::webidl::conversions;\n\n");

  out.push_str("fn binding_value_to_js<Host, R>(\n");
  out.push_str("  rt: &mut R,\n");
  out.push_str("  value: BindingValue<R::JsValue>,\n");
  out.push_str(") -> Result<R::JsValue, R::Error>\n");
  out.push_str("where\n");
  out.push_str("  R: crate::js::webidl::WebIdlBindingsRuntime<Host>,\n");
  out.push_str("{\n");
  out.push_str("  match value {\n");
  out.push_str("    BindingValue::Undefined => Ok(rt.js_undefined()),\n");
  out.push_str("    BindingValue::Null => Ok(rt.js_null()),\n");
  out.push_str("    BindingValue::Bool(b) => Ok(rt.js_bool(b)),\n");
  out.push_str("    BindingValue::Number(n) => Ok(rt.js_number(n)),\n");
  out.push_str("    BindingValue::String(s) => rt.js_string(&s),\n");
  out.push_str("    BindingValue::Object(v) => Ok(v),\n");
  out.push_str(
    "    BindingValue::Callback(_) => Err(rt.throw_type_error(\"cannot return callback handles to JavaScript\")),\n",
  );
  out.push_str("    BindingValue::Sequence(values) | BindingValue::FrozenArray(values) => {\n");
  out.push_str("      let obj = rt.create_array(values.len())?;\n");
  out.push_str("      for (idx, item) in values.into_iter().enumerate() {\n");
  out.push_str("        let key = idx.to_string();\n");
  out.push_str("        let value = binding_value_to_js::<Host, R>(rt, item)?;\n");
  out.push_str("        rt.define_data_property_str(obj, &key, value, true)?;\n");
  out.push_str("      }\n");
  out.push_str("      Ok(obj)\n");
  out.push_str("    }\n");
  out.push_str("    BindingValue::Dictionary(map) => {\n");
  out.push_str("      let obj = rt.create_object()?;\n");
  out.push_str("      for (key, item) in map {\n");
  out.push_str("        let value = binding_value_to_js::<Host, R>(rt, item)?;\n");
  out.push_str("        rt.define_data_property_str(obj, &key, value, true)?;\n");
  out.push_str("      }\n");
  out.push_str("      Ok(obj)\n");
  out.push_str("    }\n");
  out.push_str("  }\n");
  out.push_str("}\n\n");

  // Dictionary conversion helpers (sorted).
  for dict_name in &referenced_dicts {
    write_dictionary_converter(&mut out, resolved, type_ctx, dict_name)?;
  }

  // Operation shims.
  for iface in selected.values() {
    let global = is_global_iface(&iface.name);
    for (op_name, overloads) in &iface.operations {
      write_operation_wrapper(
        &mut out,
        resolved,
        &type_ctx,
        &iface.name,
        op_name,
        overloads,
        false,
        global,
        config,
      )?;
    }
    for (op_name, overloads) in &iface.static_operations {
      write_operation_wrapper(
        &mut out,
        resolved,
        &type_ctx,
        &iface.name,
        op_name,
        overloads,
        true,
        global,
        config,
      )?;
    }
    for attr in iface.attributes.values() {
      write_attribute_getter_wrapper(&mut out, &iface.name, &attr.name, false);
      if !attr.readonly {
        write_attribute_setter_wrapper(&mut out, resolved, &iface.name, &attr.name, &attr.type_, false);
      }
    }
    for attr in iface.static_attributes.values() {
      write_attribute_getter_wrapper(&mut out, &iface.name, &attr.name, true);
      if !attr.readonly {
        write_attribute_setter_wrapper(&mut out, resolved, &iface.name, &attr.name, &attr.type_, true);
      }
    }
    if !iface.constructors.is_empty() {
      write_constructor_wrapper(
        &mut out,
        resolved,
        &type_ctx,
        &iface.name,
        &iface.constructors,
        config,
      )?;
    }
  }

  // Shared illegal constructor stub for WebIDL interface objects.
  //
  // WebIDL interface objects are *always* function objects. If an interface does not declare a
  // constructor operation, the interface object must throw a TypeError when called *and* when
  // constructed (e.g. `Node()` and `new Node()`).
  //
  // For interfaces that do declare constructors, this stub is installed as `[[Call]]` so
  // `Ctor(...)` throws a TypeError, while `[[Construct]]` runs the generated constructor wrapper.
  let needs_illegal_constructor = selected.values().any(|iface| {
    let needs_ctor_obj = !iface.constructors.is_empty()
      || !iface.static_operations.is_empty()
      || !iface.static_attributes.is_empty()
      || !iface.constants.is_empty();
    needs_ctor_obj
  });
  if needs_illegal_constructor {
    out.push_str("#[allow(dead_code)]\nfn illegal_constructor<Host, R>(rt: &mut R, _host: &mut Host, _this: R::JsValue, _args: &[R::JsValue]) -> Result<R::JsValue, R::Error>\nwhere\n  R: crate::js::webidl::WebIdlBindingsRuntime<Host>,\n{\n  Err(rt.throw_type_error(\"Illegal constructor\"))\n}\n\n");
  }

  // Install entrypoint.
  out.push_str(&format!(
    "pub fn {install_fn_name}<Host, R>(rt: &mut R, host: &mut Host) -> Result<(), R::Error>\n"
  ));
  out.push_str("where\n");
  out.push_str("  R: crate::js::webidl::WebIdlBindingsRuntime<Host>,\n");
  out.push_str("  Host: WebHostBindings<R>,\n");
  out.push_str("{\n");
  out.push_str("  let global = rt.global_object()?;\n");

  // Create prototypes.
  for iface_name in selected.keys() {
    let iface = &selected[iface_name];
    if is_global_iface(&iface.name) {
      continue;
    }
    out.push_str(&format!(
      "  let proto_{snake} = rt.create_object()?;\n",
      snake = to_snake_ident(&iface.name)
    ));
  }

  if config.prototype_chains {
    // Set prototype chains.
    for iface_name in selected.keys() {
      let iface = &selected[iface_name];
      if is_global_iface(&iface.name) {
        continue;
      }
      if let Some(parent) = iface.inherits.as_deref() {
        if selected.contains_key(parent) {
          out.push_str(&format!(
            "  rt.set_prototype(proto_{child}, Some(proto_{parent}))?;\n",
            child = to_snake_ident(&iface.name),
            parent = to_snake_ident(parent)
          ));
        }
      }
    }
  }

  // Define constructors + prototypes + methods.
  for iface_name in selected.keys() {
    let iface = &selected[iface_name];
    if is_global_iface(&iface.name) {
      // Global functions live on the global object.
      for (op_name, overloads) in &iface.operations {
        let length = overloads
          .iter()
          .map(|sig| required_arg_count(&sig.arguments))
          .min()
          .unwrap_or(0);
        out.push_str(&format!(
          "  let func = rt.create_function(\"{name}\", {length}, {func}::<Host, R>)?;\n  rt.define_method(global, \"{name}\", func)?;\n",
          name = op_name,
          length = length,
          func = op_wrapper_fn_name(&iface.name, op_name)
        ));
      }
      continue;
    }

    let proto_var = format!("proto_{}", to_snake_ident(&iface.name));
    let iterable_iterator_alias = iface.iterable.as_ref().map(|it| {
      if it.key_type.is_some() {
        "entries"
      } else {
        "values"
      }
    });

    // Prototype methods.
    for (op_name, overloads) in &iface.operations {
      let length = overloads
        .iter()
        .map(|sig| required_arg_count(&sig.arguments))
        .min()
        .unwrap_or(0);
      out.push_str(&format!(
        "  let func = rt.create_function(\"{name}\", {length}, {func}::<Host, R>)?;\n  rt.define_method({proto}, \"{name}\", func)?;\n",
        proto = proto_var.as_str(),
        name = op_name,
        length = length,
        func = op_wrapper_fn_name(&iface.name, op_name)
      ));
      if iterable_iterator_alias.is_some_and(|target| target == op_name.as_str()) {
        out.push_str(&format!(
          "  let iterator_key = rt.symbol_iterator()?;\n  rt.define_data_property({proto}, iterator_key, func, false)?;\n",
          proto = proto_var.as_str()
        ));
      }
    }

    // Prototype attributes.
    for attr in iface.attributes.values() {
      out.push_str(&format!(
        "  let get = rt.create_function(\"get {name}\", 0, {getter}::<Host, R>)?;\n",
        name = attr.name,
        getter = attr_getter_fn_name(&iface.name, &attr.name, false)
      ));
      if attr.readonly {
        out.push_str("  let set = rt.js_undefined();\n");
      } else {
        out.push_str(&format!(
          "  let set = rt.create_function(\"set {name}\", 1, {setter}::<Host, R>)?;\n",
          name = attr.name,
          setter = attr_setter_fn_name(&iface.name, &attr.name, false)
        ));
      }
      out.push_str(&format!(
        "  rt.define_attribute_accessor({proto}, \"{name}\", get, set)?;\n",
        proto = proto_var.as_str(),
        name = attr.name
      ));
    }

    let needs_ctor_obj = !iface.constructors.is_empty()
      || !iface.static_operations.is_empty()
      || !iface.static_attributes.is_empty()
      || !iface.constants.is_empty();
    if !needs_ctor_obj {
      continue;
    }

    let ctor_length = iface
      .constructors
      .iter()
      .map(|sig| required_arg_count(&sig.arguments))
      .min()
      .unwrap_or(0);

    // Interface object (constructor function).
    if iface.constructors.is_empty() {
      out.push_str(&format!(
        "  let ctor_{snake} = rt.create_constructor(\"{name}\", {length}, illegal_constructor::<Host, R>, illegal_constructor::<Host, R>)?;\n",
        snake = to_snake_ident(&iface.name),
        name = iface.name.as_str(),
        length = ctor_length,
      ));
    } else {
      let ctor_fn = ctor_wrapper_fn_name(&iface.name);
      out.push_str(&format!(
        "  let ctor_{snake} = rt.create_constructor(\"{name}\", {length}, illegal_constructor::<Host, R>, {ctor_fn}::<Host, R>)?;\n",
        snake = to_snake_ident(&iface.name),
        name = iface.name.as_str(),
        length = ctor_length,
        ctor_fn = ctor_fn,
      ));
    }
    out.push_str(&format!(
      "  rt.define_constructor(global, \"{name}\", ctor_{snake}, {proto})?;\n",
      name = iface.name.as_str(),
      snake = to_snake_ident(&iface.name),
      proto = proto_var.as_str()
    ));

    // Static methods.
    for (op_name, overloads) in &iface.static_operations {
      let length = overloads
        .iter()
        .map(|sig| required_arg_count(&sig.arguments))
        .min()
        .unwrap_or(0);
      out.push_str(&format!(
        "  let func = rt.create_function(\"{name}\", {length}, {func}::<Host, R>)?;\n  rt.define_method(ctor_{snake}, \"{name}\", func)?;\n",
        snake = to_snake_ident(&iface.name),
        name = op_name,
        length = length,
        func = op_wrapper_fn_name(&iface.name, op_name)
      ));
    }

    // Static attributes.
    for attr in iface.static_attributes.values() {
      out.push_str(&format!(
        "  let get = rt.create_function(\"get {name}\", 0, {getter}::<Host, R>)?;\n",
        name = attr.name,
        getter = attr_getter_fn_name(&iface.name, &attr.name, true)
      ));
      if attr.readonly {
        out.push_str("  let set = rt.js_undefined();\n");
      } else {
        out.push_str(&format!(
          "  let set = rt.create_function(\"set {name}\", 1, {setter}::<Host, R>)?;\n",
          name = attr.name,
          setter = attr_setter_fn_name(&iface.name, &attr.name, true)
        ));
      }
      out.push_str(&format!(
        "  rt.define_attribute_accessor(ctor_{snake}, \"{name}\", get, set)?;\n",
        snake = to_snake_ident(&iface.name),
        name = attr.name
      ));
    }

    // Constants.
    for constant in iface.constants.values() {
      let expr = emit_constant_js_value_expr(&constant.value);
      out.push_str(&format!(
        "  rt.define_constant(ctor_{snake}, \"{name}\", {expr})?;\n",
        snake = to_snake_ident(&iface.name),
        name = constant.name,
        expr = expr,
      ));
    }
  }

  out.push_str("  let _ = host;\n");
  out.push_str("  Ok(())\n");
  out.push_str("}\n");

  if !out.contains("conversions::") {
    out = out.replace("use crate::js::webidl::conversions;\n\n", "");
  }

  Ok(out)
}

fn select_interfaces(
  resolved: &ResolvedWebIdlWorld,
  analyzed: &AnalyzedWebIdlWorld,
  config: &WebIdlBindingsCodegenConfig,
) -> Result<BTreeMap<String, SelectedInterface>> {
  let mut out = BTreeMap::<String, SelectedInterface>::new();

  for iface_name in &config.allow_interfaces {
    let Some(iface) = analyzed.interfaces.get(iface_name) else {
      continue;
    };

    let allow = if config.mode == WebIdlBindingsGenerationMode::Allowlist {
      config.interface_allowlist.get(iface_name)
    } else {
      None
    };

    let mut constructors: Vec<ArgumentList> = Vec::new();
    let mut operations: BTreeMap<String, Vec<OperationSig>> = BTreeMap::new();
    let mut static_operations: BTreeMap<String, Vec<OperationSig>> = BTreeMap::new();
    let mut iterable: Option<IterableInfo> = None;
    let mut attributes: BTreeMap<String, AttributeSig> = BTreeMap::new();
    let mut static_attributes: BTreeMap<String, AttributeSig> = BTreeMap::new();
    let mut constants: BTreeMap<String, ConstantSig> = BTreeMap::new();

    for member in &iface.members {
      match &member.parsed {
        InterfaceMember::Constructor { arguments } => {
          let allowed = match config.mode {
            WebIdlBindingsGenerationMode::AllMembers => true,
            WebIdlBindingsGenerationMode::Allowlist => allow.is_some_and(|a| a.constructors),
          };
          if allowed {
            constructors.push(ArgumentList {
              raw: member.raw.clone(),
              arguments: arguments.clone(),
            });
          }
        }
        InterfaceMember::Operation {
          name,
          return_type,
          arguments,
          static_,
          ..
        } => {
          let Some(op_name) = name.as_deref() else {
            continue;
          };
          let allowed = match config.mode {
            WebIdlBindingsGenerationMode::AllMembers => true,
            WebIdlBindingsGenerationMode::Allowlist => {
              allow.is_some_and(|a| a.operations.contains(op_name))
            }
          };
          if allowed {
            let sig = OperationSig {
              raw: member.raw.clone(),
              name: op_name.to_string(),
              return_type: return_type.clone(),
              arguments: arguments.clone(),
            };
            if *static_ {
              static_operations
                .entry(op_name.to_string())
                .or_default()
                .push(sig);
            } else {
              operations.entry(op_name.to_string()).or_default().push(sig);
            }
          }
        }
        InterfaceMember::Iterable {
          async_,
          key_type,
          value_type,
        } => {
          if iterable.is_some() {
            bail!(
              "interface has multiple iterable declarations (interface={}, member={})",
              iface.name,
              member.raw
            );
          }
          if *async_ {
            bail!(
              "async iterable is not supported yet (interface={}, member={})",
              iface.name,
              member.raw
            );
          }

          let value_type_str = render_idl_type(value_type);
          let value_ir = type_resolution::parse_type_with_world(resolved, &value_type_str, &[])?;
          let key_ir = match key_type {
            None => None,
            Some(ty) => {
              let key_type_str = render_idl_type(ty);
              Some(type_resolution::parse_type_with_world(
                resolved,
                &key_type_str,
                &[],
              )?)
            }
          };

          let is_pair_iterable = key_ir.is_some();
          iterable = Some(IterableInfo {
            async_: *async_,
            key_type: key_ir,
            value_type: value_ir,
          });

          // WebIDL iterable declarations synthesize default operations.
          if is_pair_iterable {
            // These methods are not explicitly listed in spec IDL sources; they are implied by the
            // `iterable<>` declaration. Emit them even in allowlist mode so interfaces like
            // URLSearchParams are spec-shaped by default.
            if operations.get("entries").is_none() {
              operations.insert(
                "entries".to_string(),
                vec![OperationSig {
                  raw: "object entries();".to_string(),
                  name: "entries".to_string(),
                  return_type: IdlType::Builtin(BuiltinType::Object),
                  arguments: Vec::new(),
                }],
              );
            }
            if operations.get("keys").is_none() {
              operations.insert(
                "keys".to_string(),
                vec![OperationSig {
                  raw: "object keys();".to_string(),
                  name: "keys".to_string(),
                  return_type: IdlType::Builtin(BuiltinType::Object),
                  arguments: Vec::new(),
                }],
              );
            }
            if operations.get("values").is_none() {
              operations.insert(
                "values".to_string(),
                vec![OperationSig {
                  raw: "object values();".to_string(),
                  name: "values".to_string(),
                  return_type: IdlType::Builtin(BuiltinType::Object),
                  arguments: Vec::new(),
                }],
              );
            }
            if operations.get("forEach").is_none() {
              operations.insert(
                "forEach".to_string(),
                vec![OperationSig {
                  raw: "undefined forEach(any callback, optional any thisArg);".to_string(),
                  name: "forEach".to_string(),
                  return_type: IdlType::Builtin(BuiltinType::Undefined),
                  arguments: vec![
                    Argument {
                      ext_attrs: Vec::new(),
                      name: "callback".to_string(),
                      type_: IdlType::Builtin(BuiltinType::Any),
                      optional: false,
                      variadic: false,
                      default: None,
                    },
                    Argument {
                      ext_attrs: Vec::new(),
                      name: "thisArg".to_string(),
                      type_: IdlType::Builtin(BuiltinType::Any),
                      optional: true,
                      variadic: false,
                      default: None,
                    },
                  ],
                }],
              );
            }
          } else if operations.get("values").is_none() {
            operations.insert(
              "values".to_string(),
              vec![OperationSig {
                raw: "object values();".to_string(),
                name: "values".to_string(),
                return_type: IdlType::Builtin(BuiltinType::Object),
                arguments: Vec::new(),
              }],
            );
          }
        }

        InterfaceMember::Attribute {
          name,
          type_,
          readonly,
          static_,
          ..
        } => {
          let allowed = match config.mode {
            WebIdlBindingsGenerationMode::AllMembers => true,
            WebIdlBindingsGenerationMode::Allowlist => {
              allow.is_some_and(|a| a.attributes.contains(name))
            }
          };
          if allowed {
            let sig = AttributeSig {
              name: name.clone(),
              type_: type_.clone(),
              readonly: *readonly,
            };
            if *static_ {
              // Attributes can't overload; keep the first definition we see.
              static_attributes.entry(name.clone()).or_insert(sig);
            } else {
              attributes.entry(name.clone()).or_insert(sig);
            }
          }
        }
        InterfaceMember::Constant { name, type_, value } => {
          // The Window bindings allowlist does not currently expose per-constant selection.
          // Include all constants for selected interfaces so common Web APIs (e.g. `Node.ELEMENT_NODE`)
          // are available without additional host dispatch.
          let allowed = match config.mode {
            WebIdlBindingsGenerationMode::AllMembers => true,
            WebIdlBindingsGenerationMode::Allowlist => true,
          };
          if allowed {
            constants.entry(name.clone()).or_insert(ConstantSig {
              name: name.clone(),
              type_: type_.clone(),
              value: value.clone(),
            });
          }
        }
        _ => {}
      }
    }

    if constructors.is_empty()
      && operations.is_empty()
      && static_operations.is_empty()
      && attributes.is_empty()
      && static_attributes.is_empty()
      && constants.is_empty()
    {
      // Allow generating prototype-only scaffolding when prototype chains are enabled.
      if config.mode == WebIdlBindingsGenerationMode::Allowlist && config.prototype_chains {
        // Keep the interface with empty member lists so it can participate in prototype chains.
      } else {
        continue;
      }
    }

    out.insert(
      iface.name.clone(),
      SelectedInterface {
        name: iface.name.clone(),
        inherits: iface.inherits.clone(),
        constructors,
        operations,
        static_operations,
        iterable,
        attributes,
        static_attributes,
        constants,
      },
    );
  }

  Ok(out)
}

fn render_idl_type(ty: &IdlType) -> String {
  match ty {
    IdlType::Builtin(b) => b.to_string(),
    IdlType::Named(name) => name.clone(),
    IdlType::Nullable(inner) => format!("{}?", render_idl_type(inner)),
    IdlType::Union(members) => {
      let mut out = String::new();
      out.push('(');
      for (idx, member) in members.iter().enumerate() {
        if idx != 0 {
          out.push_str(" or ");
        }
        out.push_str(&render_idl_type(member));
      }
      out.push(')');
      out
    }
    IdlType::Sequence(inner) => format!("sequence<{}>", render_idl_type(inner)),
    IdlType::FrozenArray(inner) => format!("FrozenArray<{}>", render_idl_type(inner)),
    IdlType::Promise(inner) => format!("Promise<{}>", render_idl_type(inner)),
    IdlType::Record { key, value } => {
      format!("record<{}, {}>", render_idl_type(key), render_idl_type(value))
    }
  }
}

fn collect_referenced_dictionaries(
  resolved: &ResolvedWebIdlWorld,
  type_ctx: &TypeContext,
  interfaces: &BTreeMap<String, SelectedInterface>,
) -> BTreeSet<String> {
  let mut referenced = BTreeSet::<String>::new();

  let mut queue = Vec::<IdlType>::new();
  for iface in interfaces.values() {
    for ctor in &iface.constructors {
      for arg in &ctor.arguments {
        queue.push(arg.type_.clone());
      }
    }
    for overloads in iface
      .operations
      .values()
      .chain(iface.static_operations.values())
    {
      for sig in overloads {
        queue.push(sig.return_type.clone());
        for arg in &sig.arguments {
          queue.push(arg.type_.clone());
        }
      }
    }
    for attr in iface
      .attributes
      .values()
      .chain(iface.static_attributes.values())
    {
      queue.push(attr.type_.clone());
    }
  }

  let mut named = BTreeSet::<String>::new();
  while let Some(ty) = queue.pop() {
    collect_named_types(&ty, &mut named);
  }

  // Fixed-point closure over typedefs and dictionaries.
  let mut pending: Vec<String> = named.into_iter().collect();
  while let Some(name) = pending.pop() {
    if referenced.contains(&name) {
      continue;
    }

    if resolved.dictionaries.contains_key(&name) {
      referenced.insert(name.clone());
      // Pull in member types.
      if let Some(members) = type_ctx.flattened_dictionary_members(&name) {
        for member in members {
          let mut names = BTreeSet::new();
          collect_named_types_ir(&member.ty, &mut names);
          for n in names {
            if !referenced.contains(&n) {
              pending.push(n);
            }
          }
        }
      }
      continue;
    }

    if resolved.typedefs.contains_key(&name) {
      if let Some(ty) = type_ctx.typedefs.get(&name) {
        let mut names = BTreeSet::new();
        collect_named_types_ir(ty, &mut names);
        for n in names {
          if !referenced.contains(&n) {
            pending.push(n);
          }
        }
      }
    }
  }

  referenced
}

fn collect_named_types(ty: &IdlType, out: &mut BTreeSet<String>) {
  match ty {
    IdlType::Named(name) => {
      out.insert(name.clone());
    }
    IdlType::Nullable(inner)
    | IdlType::Sequence(inner)
    | IdlType::FrozenArray(inner)
    | IdlType::Promise(inner) => collect_named_types(inner, out),
    IdlType::Union(members) => {
      for m in members {
        collect_named_types(m, out);
      }
    }
    IdlType::Record { key, value } => {
      collect_named_types(key, out);
      collect_named_types(value, out);
    }
    IdlType::Builtin(_) => {}
  }
}

fn collect_named_types_ir(ty: &IrIdlType, out: &mut BTreeSet<String>) {
  match ty {
    IrIdlType::Named(named) => {
      out.insert(named.name.clone());
    }
    IrIdlType::Nullable(inner)
    | IrIdlType::Sequence(inner)
    | IrIdlType::FrozenArray(inner)
    | IrIdlType::AsyncSequence(inner)
    | IrIdlType::Promise(inner)
    | IrIdlType::Annotated { inner, .. } => collect_named_types_ir(inner, out),
    IrIdlType::Union(members) => {
      for m in members {
        collect_named_types_ir(m, out);
      }
    }
    IrIdlType::Record(key, value) => {
      collect_named_types_ir(key, out);
      collect_named_types_ir(value, out);
    }
    IrIdlType::Any
    | IrIdlType::Undefined
    | IrIdlType::Boolean
    | IrIdlType::Numeric(_)
    | IrIdlType::BigInt
    | IrIdlType::String(_)
    | IrIdlType::Object
    | IrIdlType::Symbol => {}
  }
}

fn write_dictionary_converter(
  out: &mut String,
  resolved: &ResolvedWebIdlWorld,
  type_ctx: &TypeContext,
  dict_name: &str,
) -> Result<()> {
  let fn_name = format!("js_to_dict_{}", to_snake_ident(dict_name));
  out.push_str(&format!(
    "#[allow(dead_code, unused_variables)]\nfn {fn_name}<Host, R>(\n  rt: &mut R,\n  host: &mut Host,\n  value: R::JsValue,\n) -> Result<BindingValue<R::JsValue>, R::Error>\nwhere\n  R: crate::js::webidl::WebIdlBindingsRuntime<Host>,\n{{\n",
  ));
  out.push_str("  let is_missing = rt.is_undefined(value) || rt.is_null(value);\n");
  out.push_str("  if !is_missing && !rt.is_object(value) {\n");
  out.push_str(&format!(
    "    return Err(rt.throw_type_error(\"expected object for dictionary {}\"));\n",
    dict_name
  ));
  out.push_str("  }\n");
  out.push_str(
    "  let mut out_dict: BTreeMap<String, BindingValue<R::JsValue>> = BTreeMap::new();\n",
  );

  let Some(members) = type_ctx.flattened_dictionary_members(dict_name) else {
    out.push_str("  Ok(BindingValue::Dictionary(out_dict))\n");
    out.push_str("}\n\n");
    return Ok(());
  };

  for webidl_ir::DictionaryMemberSchema {
    name: member_name,
    required,
    ty,
    default,
  } in members
  {
    let member_ty = expand_typedefs_in_type(type_ctx, &ty)?;
    let conversion_expr = emit_conversion_expr_ir(resolved, type_ctx, &member_ty, "js_member_value")?;

    out.push_str("  {\n");
    out.push_str(&format!(
      "    let js_member_value = if is_missing {{\n      rt.js_undefined()\n    }} else {{\n      let key = rt.property_key({name_lit})?;\n      rt.get(value, key)?\n    }};\n",
      name_lit = rust_string_literal(&member_name)
    ));

    out.push_str("    if !rt.is_undefined(js_member_value) {\n");
    out.push_str(&format!("      let converted = {conversion_expr};\n"));
    out.push_str(&format!(
      "      out_dict.insert({name_lit}.to_string(), converted);\n",
      name_lit = rust_string_literal(&member_name)
    ));
    out.push_str("    } else {\n");

    if let Some(default) = default {
      let default_expr = emit_default_value_ir(type_ctx, &member_ty, &default).with_context(|| {
        format!("emit default value for {dict_name}.{member_name} = {default:?}")
      })?;
      out.push_str(&format!(
        "      out_dict.insert({name_lit}.to_string(), {default_expr});\n",
        name_lit = rust_string_literal(&member_name)
      ));
    } else if required {
      out.push_str(&format!(
        "      return Err(rt.throw_type_error({msg_lit}));\n",
        msg_lit = rust_string_literal(&format!(
          "Missing required dictionary member {dict_name}.{member_name}"
        ))
      ));
    }

    out.push_str("    }\n");
    out.push_str("  }\n");
  }

  out.push_str("  Ok(BindingValue::Dictionary(out_dict))\n");
  out.push_str("}\n\n");
  Ok(())
}

#[derive(Debug, Clone, Copy, Default)]
struct IrConversionState {
  clamp: bool,
  enforce_range: bool,
  legacy_null_to_empty_string: bool,
}

fn emit_conversion_expr_ir(
  _resolved: &ResolvedWebIdlWorld,
  type_ctx: &TypeContext,
  ty: &IrIdlType,
  value_ident: &str,
) -> Result<String> {
  emit_conversion_expr_ir_inner(type_ctx, ty, value_ident, IrConversionState::default())
}

fn emit_conversion_expr_ir_inner(
  type_ctx: &TypeContext,
  ty: &IrIdlType,
  value_ident: &str,
  state: IrConversionState,
) -> Result<String> {
  match ty {
    IrIdlType::Annotated { annotations, inner } => {
      let mut next = state;
      for a in annotations {
        match a {
          IrTypeAnnotation::Clamp => next.clamp = true,
          IrTypeAnnotation::EnforceRange => next.enforce_range = true,
          IrTypeAnnotation::LegacyNullToEmptyString => next.legacy_null_to_empty_string = true,
          _ => {}
        }
      }
      if next.clamp && next.enforce_range {
        bail!("[Clamp] and [EnforceRange] cannot both apply to the same type");
      }
      emit_conversion_expr_ir_inner(type_ctx, inner, value_ident, next)
    }

    IrIdlType::Undefined => Ok("BindingValue::Undefined".to_string()),
    IrIdlType::Any => {
      if state.clamp || state.enforce_range || state.legacy_null_to_empty_string {
        bail!("unexpected type annotations on `any`");
      }
      Ok(format!("BindingValue::Object({value_ident})"))
    }
    IrIdlType::Boolean => {
      if state.clamp || state.enforce_range || state.legacy_null_to_empty_string {
        bail!("unexpected type annotations on `boolean`");
      }
      Ok(format!("BindingValue::Bool(rt.to_boolean({value_ident})?)"))
    }
    IrIdlType::Numeric(n) => {
      if state.legacy_null_to_empty_string {
        bail!("unexpected type annotations on numeric type");
      }
      let int_attrs = if state.clamp || state.enforce_range {
        format!(
          "conversions::IntegerConversionAttrs {{ clamp: {}, enforce_range: {} }}",
          state.clamp, state.enforce_range
        )
      } else {
        "conversions::IntegerConversionAttrs::default()".to_string()
      };
      match n {
        IrNumericType::Byte => Ok(format!(
          "BindingValue::Number(conversions::to_byte(rt, {value_ident}, {int_attrs})? as f64)",
          value_ident = value_ident,
          int_attrs = int_attrs.clone(),
        )),
        IrNumericType::Octet => Ok(format!(
          "BindingValue::Number(conversions::to_octet(rt, {value_ident}, {int_attrs})? as f64)",
          value_ident = value_ident,
          int_attrs = int_attrs.clone(),
        )),
        IrNumericType::Short => Ok(format!(
          "BindingValue::Number(conversions::to_short(rt, {value_ident}, {int_attrs})? as f64)",
          value_ident = value_ident,
          int_attrs = int_attrs.clone(),
        )),
        IrNumericType::UnsignedShort => Ok(format!(
          "BindingValue::Number(conversions::to_unsigned_short(rt, {value_ident}, {int_attrs})? as f64)",
          value_ident = value_ident,
          int_attrs = int_attrs.clone(),
        )),
        IrNumericType::Long => Ok(format!(
          "BindingValue::Number(conversions::to_long(rt, {value_ident}, {int_attrs})? as f64)",
          value_ident = value_ident,
          int_attrs = int_attrs.clone(),
        )),
        IrNumericType::UnsignedLong => Ok(format!(
          "BindingValue::Number(conversions::to_unsigned_long(rt, {value_ident}, {int_attrs})? as f64)",
          value_ident = value_ident,
          int_attrs = int_attrs.clone(),
        )),
        IrNumericType::LongLong => Ok(format!(
          "BindingValue::Number(conversions::to_long_long(rt, {value_ident}, {int_attrs})? as f64)",
          value_ident = value_ident,
          int_attrs = int_attrs.clone(),
        )),
        IrNumericType::UnsignedLongLong => Ok(format!(
          "BindingValue::Number(conversions::to_unsigned_long_long(rt, {value_ident}, {int_attrs})? as f64)",
          value_ident = value_ident,
          int_attrs = int_attrs,
        )),
        IrNumericType::Float => {
          if state.clamp || state.enforce_range {
            bail!("[Clamp]/[EnforceRange] annotations only apply to integer numeric types");
          }
          Ok(format!(
            "BindingValue::Number(conversions::to_float(rt, {value_ident})? as f64)",
            value_ident = value_ident
          ))
        }
        IrNumericType::UnrestrictedFloat => {
          if state.clamp || state.enforce_range {
            bail!("[Clamp]/[EnforceRange] annotations only apply to integer numeric types");
          }
          Ok(format!(
            "BindingValue::Number(conversions::to_unrestricted_float(rt, {value_ident})? as f64)",
            value_ident = value_ident
          ))
        }
        IrNumericType::Double => {
          if state.clamp || state.enforce_range {
            bail!("[Clamp]/[EnforceRange] annotations only apply to integer numeric types");
          }
          Ok(format!(
            "BindingValue::Number(conversions::to_double(rt, {value_ident})?)",
            value_ident = value_ident
          ))
        }
        IrNumericType::UnrestrictedDouble => {
          if state.clamp || state.enforce_range {
            bail!("[Clamp]/[EnforceRange] annotations only apply to integer numeric types");
          }
          Ok(format!(
            "BindingValue::Number(conversions::to_unrestricted_double(rt, {value_ident})?)",
            value_ident = value_ident
          ))
        }
      }
    }
    IrIdlType::BigInt | IrIdlType::Symbol | IrIdlType::Object => {
      if state.clamp || state.enforce_range || state.legacy_null_to_empty_string {
        bail!("unexpected type annotations on non-numeric type");
      }
      Ok(format!("BindingValue::Object({value_ident})"))
    }
    IrIdlType::String(_s) => {
      if state.clamp || state.enforce_range {
        bail!("[Clamp]/[EnforceRange] annotations cannot apply to string types");
      }
      let base = format!(
        "{{ let s = rt.to_string({value_ident})?; BindingValue::String(rt.js_string_to_rust_string(s)?) }}",
        value_ident = value_ident
      );
      if state.legacy_null_to_empty_string {
        Ok(format!(
          "if rt.is_null({value_ident}) {{ BindingValue::String(String::new()) }} else {{ {base} }}",
          value_ident = value_ident,
          base = base
        ))
      } else {
        Ok(base)
      }
    }

    IrIdlType::Named(named) => {
      if state.clamp || state.enforce_range {
        bail!(
          "[Clamp]/[EnforceRange] annotations cannot apply to named type `{}`",
          named.name
        );
      }
      match named.kind {
        NamedTypeKind::Dictionary => Ok(format!(
          "js_to_dict_{}::<Host, R>(rt, host, {value_ident})?",
          to_snake_ident(&named.name),
          value_ident = value_ident
        )),
        NamedTypeKind::Enum => Ok(format!(
          "{{ let s = rt.to_string({value_ident})?; BindingValue::String(rt.js_string_to_rust_string(s)?) }}",
          value_ident = value_ident
        )),
        NamedTypeKind::Typedef => {
          if let Some(inner) = type_ctx.typedefs.get(&named.name) {
            emit_conversion_expr_ir_inner(type_ctx, inner, value_ident, state)
          } else {
            Ok(format!("BindingValue::Object({value_ident})"))
          }
        }
        _ => Ok(format!("BindingValue::Object({value_ident})")),
      }
    }

    IrIdlType::Nullable(inner) => Ok(format!(
      "if rt.is_null({value_ident}) || rt.is_undefined({value_ident}) {{ BindingValue::Null }} else {{ {inner_expr} }}",
      value_ident = value_ident,
      inner_expr = emit_conversion_expr_ir_inner(type_ctx, inner, value_ident, state)?,
    )),

    IrIdlType::Sequence(elem) => {
      if state.clamp || state.enforce_range || state.legacy_null_to_empty_string {
        bail!("unexpected type annotations on `sequence`");
      }
      emit_iterable_list_conversion_expr_ir(type_ctx, elem, value_ident, "sequence", "Sequence")
    }
    IrIdlType::FrozenArray(elem) => {
      if state.clamp || state.enforce_range || state.legacy_null_to_empty_string {
        bail!("unexpected type annotations on `FrozenArray`");
      }
      emit_iterable_list_conversion_expr_ir(
        type_ctx,
        elem,
        value_ident,
        "FrozenArray",
        "FrozenArray",
      )
    }

    IrIdlType::Union(_)
    | IrIdlType::AsyncSequence(_)
    | IrIdlType::Record(_, _)
    | IrIdlType::Promise(_) => {
      if state.clamp || state.enforce_range || state.legacy_null_to_empty_string {
        bail!("unexpected type annotations on non-primitive type");
      }
      Ok(format!("BindingValue::Object({value_ident})"))
    }
  }
}

fn emit_iterable_list_conversion_expr_ir(
  type_ctx: &TypeContext,
  elem_ty: &IrIdlType,
  value_ident: &str,
  kind_label: &str,
  out_variant: &str,
) -> Result<String> {
  let elem_expr =
    emit_conversion_expr_ir_inner(type_ctx, elem_ty, "next", IrConversionState::default())?;
  Ok(format!(
    r#"{{
  if !rt.is_object({value_ident}) {{
    return Err(rt.throw_type_error("expected object for {kind_label}"));
  }}
  rt.with_stack_roots(&[{value_ident}], |rt| {{
    let iterator_key = rt.symbol_iterator()?;
    let Some(method) = rt.get_method({value_ident}, iterator_key)? else {{
      return Err(rt.throw_type_error("{kind_label}: object is not iterable"));
    }};
    let mut iterator_record = rt.get_iterator_from_method(host, {value_ident}, method)?;
    rt.with_stack_roots(&[iterator_record.iterator, iterator_record.next_method], |rt| {{
      let mut values: Vec<BindingValue<R::JsValue>> = Vec::new();
      while let Some(next) = rt.iterator_step_value(host, &mut iterator_record)? {{
        if values.len() >= rt.limits().max_sequence_length {{
          return Err(rt.throw_range_error("{kind_label} exceeds maximum length"));
        }}
        let converted = rt.with_stack_roots(&[next], |rt| Ok({elem_expr}))?;
        values.push(converted);
      }}
      Ok(BindingValue::{out_variant}(values))
    }})
  }})?
}}"#,
    value_ident = value_ident,
    kind_label = kind_label,
    out_variant = out_variant,
    elem_expr = elem_expr,
  ))
}

fn emit_default_value_ir(
  type_ctx: &TypeContext,
  ty: &IrIdlType,
  default: &IrDefaultValue,
) -> Result<String> {
  let evaluated =
    webidl_ir::eval_default_value(ty, default, type_ctx).map_err(|e| anyhow::anyhow!("{e}"))?;
  Ok(emit_binding_value_expr_from_webidl_value(&evaluated))
}

fn emit_binding_value_expr_from_webidl_value(v: &webidl_ir::WebIdlValue) -> String {
  match v {
    webidl_ir::WebIdlValue::Undefined => "BindingValue::Undefined".to_string(),
    webidl_ir::WebIdlValue::Null => "BindingValue::Null".to_string(),
    webidl_ir::WebIdlValue::Boolean(b) => {
      format!("BindingValue::Bool({})", if *b { "true" } else { "false" })
    }

    webidl_ir::WebIdlValue::Byte(n) => {
      format!("BindingValue::Number({})", emit_f64_literal(*n as f64))
    }
    webidl_ir::WebIdlValue::Octet(n) => {
      format!("BindingValue::Number({})", emit_f64_literal(*n as f64))
    }
    webidl_ir::WebIdlValue::Short(n) => {
      format!("BindingValue::Number({})", emit_f64_literal(*n as f64))
    }
    webidl_ir::WebIdlValue::UnsignedShort(n) => {
      format!("BindingValue::Number({})", emit_f64_literal(*n as f64))
    }
    webidl_ir::WebIdlValue::Long(n) => {
      format!("BindingValue::Number({})", emit_f64_literal(*n as f64))
    }
    webidl_ir::WebIdlValue::UnsignedLong(n) => {
      format!("BindingValue::Number({})", emit_f64_literal(*n as f64))
    }
    webidl_ir::WebIdlValue::LongLong(n) => {
      format!("BindingValue::Number({})", emit_f64_literal(*n as f64))
    }
    webidl_ir::WebIdlValue::UnsignedLongLong(n) => {
      format!("BindingValue::Number({})", emit_f64_literal(*n as f64))
    }
    webidl_ir::WebIdlValue::Float(n) | webidl_ir::WebIdlValue::UnrestrictedFloat(n) => {
      format!("BindingValue::Number({})", emit_f64_literal(*n as f64))
    }
    webidl_ir::WebIdlValue::Double(n) | webidl_ir::WebIdlValue::UnrestrictedDouble(n) => {
      format!("BindingValue::Number({})", emit_f64_literal(*n))
    }

    webidl_ir::WebIdlValue::String(s) | webidl_ir::WebIdlValue::Enum(s) => {
      format!("BindingValue::String({}.to_string())", rust_string_literal(s))
    }

    webidl_ir::WebIdlValue::Sequence { values, .. } => {
      if values.is_empty() {
        "BindingValue::Sequence(Vec::new())".to_string()
      } else {
        let items = values
          .iter()
          .map(emit_binding_value_expr_from_webidl_value)
          .collect::<Vec<_>>()
          .join(", ");
        format!("BindingValue::Sequence(vec![{items}])")
      }
    }

    webidl_ir::WebIdlValue::Record { entries, .. } => {
      if entries.is_empty() {
        "BindingValue::Dictionary(BTreeMap::new())".to_string()
      } else {
        let mut out = String::new();
        out.push_str("{\n  let mut map = BTreeMap::new();\n");
        for (k, v) in entries.iter().cloned() {
          let value_expr = emit_binding_value_expr_from_webidl_value(&v);
          out.push_str(&format!(
            "  map.insert({key}.to_string(), {value_expr});\n",
            key = rust_string_literal(&k)
          ));
        }
        out.push_str("  BindingValue::Dictionary(map)\n}");
        out
      }
    }

    webidl_ir::WebIdlValue::Dictionary { members, .. } => {
      if members.is_empty() {
        "BindingValue::Dictionary(BTreeMap::new())".to_string()
      } else {
        let mut out = String::new();
        out.push_str("{\n  let mut map = BTreeMap::new();\n");
        for (k, v) in members {
          let value_expr = emit_binding_value_expr_from_webidl_value(v);
          out.push_str(&format!(
            "  map.insert({key}.to_string(), {value_expr});\n",
            key = rust_string_literal(k)
          ));
        }
        out.push_str("  BindingValue::Dictionary(map)\n}");
        out
      }
    }

    webidl_ir::WebIdlValue::Union { value, .. } => emit_binding_value_expr_from_webidl_value(value),
    webidl_ir::WebIdlValue::PlatformObject(_) => "BindingValue::Undefined".to_string(),
  }
}

fn emit_f64_literal(value: f64) -> String {
  if value.is_nan() {
    "f64::NAN".to_string()
  } else if value.is_infinite() {
    if value.is_sign_negative() {
      "f64::NEG_INFINITY".to_string()
    } else {
      "f64::INFINITY".to_string()
    }
  } else {
    format!("{value:?}")
  }
}

fn ast_idl_type_to_webidl_ir_src(ty: &IdlType) -> String {
  match ty {
    IdlType::Builtin(b) => b.to_string(),
    IdlType::Named(name) => name.clone(),
    IdlType::Nullable(inner) => format!("{}?", ast_idl_type_to_webidl_ir_src(inner)),
    IdlType::Union(members) => format!(
      "({})",
      members
        .iter()
        .map(ast_idl_type_to_webidl_ir_src)
        .collect::<Vec<_>>()
        .join(" or ")
    ),
    IdlType::Sequence(inner) => format!("sequence<{}>", ast_idl_type_to_webidl_ir_src(inner)),
    IdlType::FrozenArray(inner) => format!("FrozenArray<{}>", ast_idl_type_to_webidl_ir_src(inner)),
    IdlType::Promise(inner) => format!("Promise<{}>", ast_idl_type_to_webidl_ir_src(inner)),
    IdlType::Record { key, value } => format!(
      "record<{}, {}>",
      ast_idl_type_to_webidl_ir_src(key),
      ast_idl_type_to_webidl_ir_src(value)
    ),
  }
}

fn idl_literal_to_webidl_ir_default_value(lit: &IdlLiteral) -> Option<webidl_ir::DefaultValue> {
  use webidl_ir::{DefaultValue, NumericLiteral};
  match lit {
    IdlLiteral::Null => Some(DefaultValue::Null),
    IdlLiteral::Undefined => Some(DefaultValue::Undefined),
    IdlLiteral::Boolean(b) => Some(DefaultValue::Boolean(*b)),
    IdlLiteral::Number(n) => Some(DefaultValue::Number(NumericLiteral::Integer(n.clone()))),
    IdlLiteral::String(s) => Some(DefaultValue::String(s.clone())),
    IdlLiteral::EmptyObject => Some(DefaultValue::EmptyDictionary),
    IdlLiteral::EmptyArray => Some(DefaultValue::EmptySequence),
    IdlLiteral::Identifier(_) => None,
  }
}

fn build_overload_ir_operation_set(
  resolved: &ResolvedWebIdlWorld,
  type_ctx: &webidl_ir::TypeContext,
  interface: &str,
  op_name: &str,
  overloads: &[OperationSig],
) -> Result<Vec<crate::webidl::overload_ir::Overload>> {
  use crate::webidl::overload_ir::{Optionality, Origin, Overload, OverloadArgument};

  let display_name = format!("{interface}.{op_name}");
  let mut out = Vec::with_capacity(overloads.len());

  for sig in overloads {
    let mut args = Vec::with_capacity(sig.arguments.len());
    for arg in &sig.arguments {
      let ty_src = ast_idl_type_to_webidl_ir_src(&arg.type_);
      let ty = crate::webidl::type_resolution::parse_type_with_world_and_typedefs(
        resolved,
        type_ctx,
        &ty_src,
        &arg.ext_attrs,
        true,
      )
      .with_context(|| {
        format!(
          "parse argument type for `{}` overload `{}` argument `{}`",
          display_name, sig.raw, arg.name
        )
      })?;

      let optionality = if arg.variadic {
        Optionality::Variadic
      } else if arg.optional || arg.default.is_some() {
        Optionality::Optional
      } else {
        Optionality::Required
      };

      args.push(OverloadArgument {
        name: Some(arg.name.clone()),
        ty,
        optionality,
        default: arg
          .default
          .as_ref()
          .and_then(idl_literal_to_webidl_ir_default_value),
      });
    }

    out.push(Overload {
      name: display_name.clone(),
      arguments: args,
      origin: Some(Origin {
        interface: interface.to_string(),
        raw_member: sig.raw.clone(),
      }),
    });
  }

  Ok(out)
}

fn build_overload_ir_constructor_set(
  resolved: &ResolvedWebIdlWorld,
  type_ctx: &webidl_ir::TypeContext,
  interface: &str,
  overloads: &[ArgumentList],
) -> Result<Vec<crate::webidl::overload_ir::Overload>> {
  use crate::webidl::overload_ir::{Optionality, Origin, Overload, OverloadArgument};

  let display_name = format!("{interface}.constructor");
  let mut out = Vec::with_capacity(overloads.len());

  for sig in overloads {
    let mut args = Vec::with_capacity(sig.arguments.len());
    for arg in &sig.arguments {
      let ty_src = ast_idl_type_to_webidl_ir_src(&arg.type_);
      let ty = crate::webidl::type_resolution::parse_type_with_world_and_typedefs(
        resolved,
        type_ctx,
        &ty_src,
        &arg.ext_attrs,
        true,
      )
      .with_context(|| {
        format!(
          "parse argument type for `{}` overload `{}` argument `{}`",
          display_name, sig.raw, arg.name
        )
      })?;

      let optionality = if arg.variadic {
        Optionality::Variadic
      } else if arg.optional || arg.default.is_some() {
        Optionality::Optional
      } else {
        Optionality::Required
      };

      args.push(OverloadArgument {
        name: Some(arg.name.clone()),
        ty,
        optionality,
        default: arg
          .default
          .as_ref()
          .and_then(idl_literal_to_webidl_ir_default_value),
      });
    }

    out.push(Overload {
      name: display_name.clone(),
      arguments: args,
      origin: Some(Origin {
        interface: interface.to_string(),
        raw_member: sig.raw.clone(),
      }),
    });
  }

  Ok(out)
}

fn format_overload_signature(
  display_name: &str,
  overload: &crate::webidl::overload_ir::Overload,
) -> String {
  use crate::webidl::overload_ir::Optionality;
  let args = overload
    .arguments
    .iter()
    .map(|a| {
      let mut s = String::new();
      match a.optionality {
        Optionality::Optional => s.push_str("optional "),
        Optionality::Required | Optionality::Variadic => {}
      }
      s.push_str(&a.ty.to_string());
      if a.optionality == Optionality::Variadic {
        s.push_str("...");
      }
      if a.optionality == Optionality::Optional && a.default.is_some() {
        s.push_str(" = <default>");
      }
      s
    })
    .collect::<Vec<_>>()
    .join(", ");
  format!("{display_name}({args})")
}

fn format_overload_validation_failure(
  diags: Vec<crate::webidl::overload_ir::Diagnostic>,
) -> String {
  let mut out = String::new();
  for (idx, diag) in diags.into_iter().enumerate() {
    if idx != 0 {
      out.push('\n');
    }
    out.push_str(&diag.message);
    if !diag.origins.is_empty() {
      out.push_str("\nOrigins:");
      for origin in diag.origins {
        out.push_str(&format!(
          "\n  - {}: {}",
          origin.interface, origin.raw_member
        ));
      }
    }
  }
  out
}

fn type_category_fast_path(
  ty: &webidl_ir::IdlType,
) -> crate::webidl::overload_ir::TypeCategoryFastPath {
  use crate::webidl::overload_ir::TypeCategoryFastPath;
  let flattened = ty.flattened_union_member_types();
  TypeCategoryFastPath {
    category: ty.category_for_distinguishability(),
    innermost_named_type: match ty.innermost_type() {
      webidl_ir::IdlType::Named(named) => Some(named.clone()),
      _ => None,
    },
    includes_nullable_type: ty.includes_nullable_type(),
    includes_undefined: ty.includes_undefined(),
    flattened_union_member_categories: flattened
      .iter()
      .map(|t| t.category_for_distinguishability())
      .collect(),
    flattened_union_member_types: flattened,
  }
}

fn fast_path_matches_category(
  fp: &crate::webidl::overload_ir::TypeCategoryFastPath,
  cat: webidl_ir::DistinguishabilityCategory,
) -> bool {
  if fp.category == Some(cat) {
    return true;
  }
  fp.flattened_union_member_categories
    .iter()
    .copied()
    .any(|c| c == Some(cat))
}

fn fast_path_matches_nullable_dictionary(
  fp: &crate::webidl::overload_ir::TypeCategoryFastPath,
) -> bool {
  if fp.includes_nullable_type {
    return true;
  }
  fp.flattened_union_member_types.iter().any(|t| {
    matches!(
      t.innermost_type(),
      webidl_ir::IdlType::Named(webidl_ir::NamedType {
        kind: webidl_ir::NamedTypeKind::Dictionary,
        ..
      })
    )
  })
}

fn interface_ids_for_fast_path(
  fp: &crate::webidl::overload_ir::TypeCategoryFastPath,
) -> Vec<(String, u32)> {
  fn interface_id_from_name_u32(name: &str) -> u32 {
    // Must match `webidl_js_runtime::interface_id_from_name` (FNV-1a 32-bit).
    let mut hash: u32 = 0x811c_9dc5;
    for &b in name.as_bytes() {
      hash ^= b as u32;
      hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
  }

  let mut out: Vec<(String, u32)> = Vec::new();

  for t in &fp.flattened_union_member_types {
    let webidl_ir::IdlType::Named(named) = t.innermost_type() else {
      continue;
    };
    if named.kind != webidl_ir::NamedTypeKind::Interface {
      continue;
    }
    if out.iter().any(|(n, _)| n == &named.name) {
      continue;
    }
    out.push((named.name.clone(), interface_id_from_name_u32(&named.name)));
  }

  if out.is_empty() {
    // Non-union types store the innermost named type separately; unions do not.
    if let Some(named) = fp
      .innermost_named_type
      .as_ref()
      .filter(|n| n.kind == webidl_ir::NamedTypeKind::Interface)
    {
      out.push((named.name.clone(), interface_id_from_name_u32(&named.name)));
    }
  }

  out
}

fn compute_codegen_overload_dispatch_plan<C: crate::webidl::overload_ir::WorldContext>(
  overloads: &[crate::webidl::overload_ir::Overload],
  world_ctx: &C,
) -> crate::webidl::overload_ir::OverloadDispatchPlan {
  use crate::webidl::overload_ir::{
    compute_effective_overload_set, distinguishing_argument_index, EffectiveOverloadEntry,
    Optionality, OverloadDispatchGroup, OverloadDispatchPlan,
  };
  use std::collections::BTreeMap;

  let max_declared = overloads
    .iter()
    .map(|o| o.arguments.len())
    .max()
    .unwrap_or(0);
  let has_variadic = overloads.iter().any(|o| {
    o.arguments
      .last()
      .is_some_and(|a| a.optionality == Optionality::Variadic)
  });
  // If a variadic overload is present, generate one extra effective argument-count bucket so runtime
  // dispatch can treat `args.len() > max_declared` as "variadic call" without needing infinite
  // precomputation.
  let n_for_effective = if has_variadic {
    max_declared.saturating_add(1)
  } else {
    max_declared
  };

  let effective = compute_effective_overload_set(overloads, n_for_effective);

  let mut by_len: BTreeMap<usize, Vec<EffectiveOverloadEntry>> = BTreeMap::new();
  for entry in &effective.items {
    by_len
      .entry(entry.type_list.len())
      .or_default()
      .push(entry.clone());
  }

  let mut groups = Vec::with_capacity(by_len.len());
  for (argument_count, entries) in by_len {
    let distinguishing_argument_index = distinguishing_argument_index(&entries, world_ctx);
    let distinguishing_argument_types = if let Some(d) = distinguishing_argument_index {
      entries
        .iter()
        .map(|e| type_category_fast_path(&e.type_list[d]))
        .collect()
    } else {
      Vec::new()
    };
    groups.push(OverloadDispatchGroup {
      argument_count,
      entries,
      distinguishing_argument_index,
      distinguishing_argument_types,
    });
  }

  OverloadDispatchPlan { effective, groups }
}

fn emit_no_matching_overload_expr(
  display_name: &str,
  candidate_sigs: &[String],
  args_ident: &str,
) -> String {
  let mut msg = format!("No matching overload for {display_name} with {{}} arguments.");
  if !candidate_sigs.is_empty() {
    msg.push_str("\nCandidates:");
    for sig in candidate_sigs {
      msg.push_str("\n  - ");
      msg.push_str(sig);
    }
  }
  format!(
    "Err(rt.throw_type_error(&format!({msg_lit}, {args_ident}.len())))",
    msg_lit = rust_string_literal(&msg),
    args_ident = args_ident
  )
}

fn write_attribute_getter_wrapper(out: &mut String, interface: &str, attr_name: &str, is_static: bool) {
  let fn_name = attr_getter_fn_name(interface, attr_name, is_static);
  out.push_str(&format!(
    "#[allow(dead_code)]\nfn {fn_name}<Host, R>(rt: &mut R, host: &mut Host, this: R::JsValue, _args: &[R::JsValue]) -> Result<R::JsValue, R::Error>\nwhere\n  R: crate::js::webidl::WebIdlBindingsRuntime<Host>,\n  Host: WebHostBindings<R>,\n{{\n",
  ));

  let receiver_expr = if interface == "Window" || is_static {
    "None"
  } else {
    "Some(this)"
  };
  if receiver_expr == "Some(this)" {
    out.push_str("  if !rt.is_object(this) {\n");
    out.push_str("    return Err(rt.throw_type_error(\"Illegal invocation\"));\n");
    out.push_str("  }\n");
  }

  out.push_str(&format!(
    "  let result = host.get_attribute(rt, {receiver_expr}, {iface_lit}, {attr_lit})?;\n",
    receiver_expr = receiver_expr,
    iface_lit = rust_string_literal(interface),
    attr_lit = rust_string_literal(attr_name),
  ));
  out.push_str("  binding_value_to_js::<Host, R>(rt, result)\n");
  out.push_str("}\n\n");
}

fn write_attribute_setter_wrapper(
  out: &mut String,
  resolved: &ResolvedWebIdlWorld,
  interface: &str,
  attr_name: &str,
  ty: &IdlType,
  is_static: bool,
) {
  let fn_name = attr_setter_fn_name(interface, attr_name, is_static);
  out.push_str(&format!(
    "#[allow(dead_code)]\nfn {fn_name}<Host, R>(rt: &mut R, host: &mut Host, this: R::JsValue, args: &[R::JsValue]) -> Result<R::JsValue, R::Error>\nwhere\n  R: crate::js::webidl::WebIdlBindingsRuntime<Host>,\n  Host: WebHostBindings<R>,\n{{\n",
  ));

  let receiver_expr = if interface == "Window" || is_static {
    "None"
  } else {
    "Some(this)"
  };
  if receiver_expr == "Some(this)" {
    out.push_str("  if !rt.is_object(this) {\n");
    out.push_str("    return Err(rt.throw_type_error(\"Illegal invocation\"));\n");
    out.push_str("  }\n");
  }

  out.push_str("  let v0 = if args.len() > 0 { args[0] } else { rt.js_undefined() };\n");
  out.push_str(&format!(
    "  let converted = {};\n",
    emit_conversion_expr(resolved, ty, &[], "v0")
  ));
  out.push_str(&format!(
    "  host.set_attribute(rt, {receiver_expr}, {iface_lit}, {attr_lit}, converted)?;\n",
    receiver_expr = receiver_expr,
    iface_lit = rust_string_literal(interface),
    attr_lit = rust_string_literal(attr_name),
  ));
  out.push_str("  Ok(rt.js_undefined())\n");
  out.push_str("}\n\n");
}

fn write_operation_wrapper(
  out: &mut String,
  resolved: &ResolvedWebIdlWorld,
  type_ctx: &webidl_ir::TypeContext,
  interface: &str,
  op_name: &str,
  overloads: &[OperationSig],
  is_static: bool,
  is_global: bool,
  config: &WebIdlBindingsCodegenConfig,
) -> Result<()> {
  let _ = config;
  let fn_name = op_wrapper_fn_name(interface, op_name);
  out.push_str(&format!(
    "#[allow(dead_code)]\nfn {fn_name}<Host, R>(rt: &mut R, host: &mut Host, this: R::JsValue, args: &[R::JsValue]) -> Result<R::JsValue, R::Error>\nwhere\n  R: crate::js::webidl::WebIdlBindingsRuntime<Host>,\n  Host: WebHostBindings<R>,\n{{\n",
  ));

  let receiver_expr = if is_global || is_static {
    "None"
  } else {
    "Some(this)"
  };

  if overloads.len() == 1 {
    out.push_str(&indent_lines(
      &emit_overload_call(
        resolved,
        interface,
        op_name,
        receiver_expr,
        0,
        &overloads[0].arguments,
      ),
      2,
    ));
    out.push_str("}\n\n");
    return Ok(());
  }

  let overload_ir_set =
    build_overload_ir_operation_set(resolved, type_ctx, interface, op_name, overloads)
      .with_context(|| format!("build overload-set IR for {interface}.{op_name}"))?;

  if let Err(diags) = crate::webidl::overload_ir::validate_overload_set(&overload_ir_set, resolved)
  {
    bail!(
      "WebIDL overload validation failed for {interface}.{op_name}:\n{}",
      format_overload_validation_failure(diags)
    );
  }

  let plan = compute_codegen_overload_dispatch_plan(&overload_ir_set, resolved);

  let display_name = format!("{interface}.{op_name}");
  let mut candidate_sigs = overload_ir_set
    .iter()
    .map(|o| format_overload_signature(&display_name, o))
    .collect::<Vec<_>>();
  candidate_sigs.sort();
  candidate_sigs.dedup();
  let no_match_expr = emit_no_matching_overload_expr(&display_name, &candidate_sigs, "args");

  let max_argc = plan
    .groups
    .iter()
    .map(|g| g.argument_count)
    .max()
    .unwrap_or(0);

  out.push_str(&format!(
    "  let argcount = std::cmp::min(args.len(), {max_argc});\n  match argcount {{\n",
    max_argc = max_argc
  ));

  for group in &plan.groups {
    out.push_str(&format!("    {} => {{\n", group.argument_count));

    if group.entries.len() == 1 {
      let overload_idx = group.entries[0].callable_id;
      let call = emit_overload_call(
        resolved,
        interface,
        op_name,
        receiver_expr,
        overload_idx,
        &overloads[overload_idx].arguments,
      );
      out.push_str(&indent_lines(&call, 6));
      out.push_str("    },\n");
      continue;
    }

    let d = group.distinguishing_argument_index.with_context(|| {
      format!(
        "missing distinguishing argument index for {display_name} argcount={}",
        group.argument_count
      )
    })?;
    out.push_str(&format!("      let v = args[{d}];\n", d = d));

    let mut optional_candidate: Option<usize> = None;
    let mut nullable_dict_candidate: Option<usize> = None;
    let mut string_candidate: Option<usize> = None;
    let mut callback_candidate: Option<usize> = None;
    let mut async_sequence_candidate: Option<usize> = None;
    let mut sequence_candidate: Option<usize> = None;
    let mut object_like_candidate: Option<usize> = None;
    let mut boolean_candidate: Option<usize> = None;
    let mut numeric_candidate: Option<usize> = None;
    let mut bigint_candidate: Option<usize> = None;
    let mut symbol_candidate: Option<usize> = None;
    let mut interface_like_candidates: Vec<(usize, Vec<(String, u32)>)> = Vec::new();

    for (idx, entry) in group.entries.iter().enumerate() {
      let fp = &group.distinguishing_argument_types[idx];
      let overload_idx = entry.callable_id;

      if entry.optionality_list.get(d) == Some(&crate::webidl::overload_ir::Optionality::Optional) {
        if let Some(prev) = optional_candidate.replace(overload_idx) {
          if prev != overload_idx {
            bail!(
              "ambiguous overload dispatch for {display_name}: multiple optional overloads at distinguishing index {d} (argcount={})",
              group.argument_count
            );
          }
        }
      }

      if fast_path_matches_nullable_dictionary(fp) {
        if let Some(prev) = nullable_dict_candidate.replace(overload_idx) {
          if prev != overload_idx {
            bail!(
              "ambiguous overload dispatch for {display_name}: multiple nullable/dictionary-like overloads at distinguishing index {d} (argcount={})",
              group.argument_count
            );
          }
        }
      }

      if fast_path_matches_category(fp, webidl_ir::DistinguishabilityCategory::String) {
        if let Some(prev) = string_candidate.replace(overload_idx) {
          if prev != overload_idx {
            bail!(
              "ambiguous overload dispatch for {display_name}: multiple string overloads at distinguishing index {d} (argcount={})",
              group.argument_count
            );
          }
        }
      }

      if fast_path_matches_category(fp, webidl_ir::DistinguishabilityCategory::CallbackFunction) {
        callback_candidate = Some(overload_idx);
      }
      if fast_path_matches_category(fp, webidl_ir::DistinguishabilityCategory::AsyncSequence) {
        async_sequence_candidate = Some(overload_idx);
      }
      if fast_path_matches_category(fp, webidl_ir::DistinguishabilityCategory::SequenceLike) {
        sequence_candidate = Some(overload_idx);
      }

      if fast_path_matches_category(fp, webidl_ir::DistinguishabilityCategory::Object)
        || fast_path_matches_category(fp, webidl_ir::DistinguishabilityCategory::DictionaryLike)
      {
        object_like_candidate = Some(overload_idx);
      }

      if fast_path_matches_category(fp, webidl_ir::DistinguishabilityCategory::Boolean) {
        boolean_candidate = Some(overload_idx);
      }
      if fast_path_matches_category(fp, webidl_ir::DistinguishabilityCategory::Numeric) {
        numeric_candidate = Some(overload_idx);
      }
      if fast_path_matches_category(fp, webidl_ir::DistinguishabilityCategory::BigInt) {
        bigint_candidate = Some(overload_idx);
      }
      if fast_path_matches_category(fp, webidl_ir::DistinguishabilityCategory::Symbol) {
        symbol_candidate = Some(overload_idx);
      }

      if fast_path_matches_category(fp, webidl_ir::DistinguishabilityCategory::InterfaceLike) {
        interface_like_candidates.push((overload_idx, interface_ids_for_fast_path(fp)));
      }
    }

    let mut emit_call = |overload_idx: usize| -> String {
      emit_overload_call(
        resolved,
        interface,
        op_name,
        receiver_expr,
        overload_idx,
        &overloads[overload_idx].arguments,
      )
    };

    // Emit spec-shaped dispatch (WebIDL overload resolution algorithm, simplified to the
    // distinguishability categories we support in this generator).
    let mut if_chain = String::new();

    // 1. Optional undefined special-case.
    if let Some(oidx) = optional_candidate {
      if_chain.push_str("      if rt.is_undefined(v) {\n");
      if_chain.push_str(&indent_lines(&emit_call(oidx), 8));
      if_chain.push_str("      }");
    }

    // 2. Nullable/dictionary special-case (null or undefined).
    if let Some(oidx) = nullable_dict_candidate {
      if if_chain.is_empty() {
        if_chain.push_str("      if rt.is_null(v) || rt.is_undefined(v) {\n");
      } else {
        if_chain.push_str(" else if rt.is_null(v) || rt.is_undefined(v) {\n");
      }
      if_chain.push_str(&indent_lines(&emit_call(oidx), 8));
      if_chain.push_str("      }");
    }

    // 3. String/String-object fast-path (prevents string objects from being treated as generic objects).
    if let Some(oidx) = string_candidate {
      if if_chain.is_empty() {
        if_chain.push_str("      if rt.is_string(v) || rt.is_string_object(v) {\n");
      } else {
        if_chain.push_str(" else if rt.is_string(v) || rt.is_string_object(v) {\n");
      }
      if_chain.push_str(&indent_lines(&emit_call(oidx), 8));
      if_chain.push_str("      }");
    }

    // 4. Platform object + interface-like fast-path.
    for (oidx, iface_ids) in &interface_like_candidates {
      if iface_ids.is_empty() {
        continue;
      }
      let mut cond = String::new();
      for (idx, (_name, id)) in iface_ids.iter().enumerate() {
        if idx != 0 {
          cond.push_str(" || ");
        }
        cond.push_str(&format!(
          "rt.implements_interface(v, crate::js::webidl::InterfaceId(0x{hash:08x}))",
          hash = id
        ));
      }
      let cond = format!("rt.is_platform_object(v) && ({cond})");
      if if_chain.is_empty() {
        if_chain.push_str(&format!("      if {cond} {{\n"));
      } else {
        if_chain.push_str(&format!(" else if {cond} {{\n"));
      }
      if_chain.push_str(&indent_lines(&emit_call(*oidx), 8));
      if_chain.push_str("      }");
    }

    // 5. Callable / callback function fast-path.
    if let Some(oidx) = callback_candidate {
      if if_chain.is_empty() {
        if_chain.push_str("      if rt.is_callable(v) {\n");
      } else {
        if_chain.push_str(" else if rt.is_callable(v) {\n");
      }
      if_chain.push_str(&indent_lines(&emit_call(oidx), 8));
      if_chain.push_str("      }");
    }

    // 6. Async sequence fast-path (iterable object with @@asyncIterator or @@iterator).
    if let Some(oidx) = async_sequence_candidate {
      let cond = "rt.is_object(v) && {\n        let async_iter = rt.symbol_async_iterator()?;\n        let iter = rt.symbol_iterator()?;\n        let mut m = rt.get_method(v, async_iter)?;\n        if m.is_none() {\n          m = rt.get_method(v, iter)?;\n        }\n        m.is_some()\n      }"
        .replace('\t', "  ");
      if if_chain.is_empty() {
        if_chain.push_str(&format!("      if {cond} {{\n"));
      } else {
        if_chain.push_str(&format!(" else if {cond} {{\n"));
      }
      if_chain.push_str(&indent_lines(&emit_call(oidx), 8));
      if_chain.push_str("      }");
    }

    // 7. Sequence fast-path (iterable object with @@iterator).
    if let Some(oidx) = sequence_candidate {
      let cond = "rt.is_object(v) && {\n        let iter = rt.symbol_iterator()?;\n        rt.get_method(v, iter)?.is_some()\n      }"
        .replace('\t', "  ");
      if if_chain.is_empty() {
        if_chain.push_str(&format!("      if {cond} {{\n"));
      } else {
        if_chain.push_str(&format!(" else if {cond} {{\n"));
      }
      if_chain.push_str(&indent_lines(&emit_call(oidx), 8));
      if_chain.push_str("      }");
    }

    // 8. Object/dictionary-like fast-path.
    if let Some(oidx) = object_like_candidate {
      if if_chain.is_empty() {
        if_chain.push_str("      if rt.is_object(v) {\n");
      } else {
        if_chain.push_str(" else if rt.is_object(v) {\n");
      }
      if_chain.push_str(&indent_lines(&emit_call(oidx), 8));
      if_chain.push_str("      }");
    }

    // 9. Primitive scalar fast-paths.
    if let Some(oidx) = boolean_candidate {
      if if_chain.is_empty() {
        if_chain.push_str("      if rt.is_boolean(v) {\n");
      } else {
        if_chain.push_str(" else if rt.is_boolean(v) {\n");
      }
      if_chain.push_str(&indent_lines(&emit_call(oidx), 8));
      if_chain.push_str("      }");
    }
    if let Some(oidx) = numeric_candidate {
      if if_chain.is_empty() {
        if_chain.push_str("      if rt.is_number(v) {\n");
      } else {
        if_chain.push_str(" else if rt.is_number(v) {\n");
      }
      if_chain.push_str(&indent_lines(&emit_call(oidx), 8));
      if_chain.push_str("      }");
    }
    if let Some(oidx) = bigint_candidate {
      if if_chain.is_empty() {
        if_chain.push_str("      if rt.is_bigint(v) {\n");
      } else {
        if_chain.push_str(" else if rt.is_bigint(v) {\n");
      }
      if_chain.push_str(&indent_lines(&emit_call(oidx), 8));
      if_chain.push_str("      }");
    }
    if let Some(oidx) = symbol_candidate {
      if if_chain.is_empty() {
        if_chain.push_str("      if rt.is_symbol(v) {\n");
      } else {
        if_chain.push_str(" else if rt.is_symbol(v) {\n");
      }
      if_chain.push_str(&indent_lines(&emit_call(oidx), 8));
      if_chain.push_str("      }");
    }

    // 10. Fallthrough by category (string > numeric > boolean > bigint).
    let fallback_expr = if let Some(oidx) = string_candidate {
      emit_call(oidx)
    } else if let Some(oidx) = numeric_candidate {
      emit_call(oidx)
    } else if let Some(oidx) = boolean_candidate {
      emit_call(oidx)
    } else if let Some(oidx) = bigint_candidate {
      emit_call(oidx)
    } else {
      no_match_expr.clone()
    };

    if if_chain.is_empty() {
      // No conditional branches matched anything; use fallthrough directly.
      out.push_str(&indent_lines(&fallback_expr, 6));
      out.push_str("    },\n");
      continue;
    }

    if_chain.push_str(" else {\n");
    if_chain.push_str(&indent_lines(&fallback_expr, 8));
    if_chain.push_str("      }\n");

    out.push_str(&if_chain);
    out.push('\n');
    out.push_str("    },\n");
  }

  out.push_str(&format!(
    "    _ => {no_match_expr},\n  }}\n",
    no_match_expr = no_match_expr
  ));
  out.push_str("}\n\n");
  Ok(())
}

fn emit_overload_call(
  resolved: &ResolvedWebIdlWorld,
  interface: &str,
  operation: &str,
  receiver_expr: &str,
  overload_idx: usize,
  arguments: &[Argument],
) -> String {
  let mut out = String::new();
  out.push_str("{\n");
  out.push_str("  let mut converted_args: Vec<BindingValue<R::JsValue>> = Vec::new();\n");
  for (idx, arg) in arguments.iter().enumerate() {
    if arg.variadic {
      out.push_str(&format!(
        "  let mut rest: Vec<BindingValue<R::JsValue>> = Vec::new();\n  for v in args.iter().copied().skip({idx}) {{\n    rest.push({});\n  }}\n  converted_args.push(BindingValue::Sequence(rest));\n",
        emit_conversion_expr(resolved, &arg.type_, &arg.ext_attrs, "v"),
      ));
      break;
    }

    out.push_str(&format!(
      "  let v{idx} = if args.len() > {idx} {{ args[{idx}] }} else {{ rt.js_undefined() }};\n",
      idx = idx
    ));
    let expr = emit_conversion_expr_for_optional(resolved, arguments, idx, arg, &format!("v{idx}"));
    out.push_str(&format!("  converted_args.push({expr});\n"));
  }
  out.push_str(&format!(
    "  let result = host.call_operation(rt, {receiver_expr}, {iface_lit}, {op_lit}, {overload_idx}, converted_args)?;\n",
    receiver_expr = receiver_expr,
    iface_lit = rust_string_literal(interface),
    op_lit = rust_string_literal(operation),
    overload_idx = overload_idx
  ));
  out.push_str("  binding_value_to_js::<Host, R>(rt, result)\n");
  out.push_str("}\n");
  out
}

fn emit_conversion_expr_for_optional(
  resolved: &ResolvedWebIdlWorld,
  _all_args: &[Argument],
  _idx: usize,
  arg: &Argument,
  value_ident: &str,
) -> String {
  // Treat `optional` and `= default` as optional in this generator.
  let is_optional = arg.optional || arg.default.is_some();
  if !is_optional {
    return emit_conversion_expr(resolved, &arg.type_, &arg.ext_attrs, value_ident);
  }

  // Dictionary arguments: even when the argument itself is optional/defaulted, WebIDL still runs
  // dictionary conversion on `undefined`/`null` (treating them as a "missing dictionary object") so
  // per-member defaulting / required-member checks are applied. Our generic optional/defaulted
  // argument handling short-circuits `undefined` to the argument default, so dictionary arguments
  // must bypass that and always go through the dictionary converter.
  if let IdlType::Named(name) = &arg.type_ {
    if resolved.dictionaries.contains_key(name) {
      return format!(
        "js_to_dict_{}::<Host, R>(rt, host, {value_ident})?",
        to_snake_ident(name),
        value_ident = value_ident
      );
    }
  }

  // Optional/defaulted union arguments that default to `{}` and include a dictionary type: treat an
  // `undefined` argument as the empty dictionary *value* (with defaults applied).
  if matches!(arg.default, Some(IdlLiteral::EmptyObject)) {
    if let IdlType::Union(members) = &arg.type_ {
      if let Some(dict_name) = members.iter().find_map(|m| match m {
        IdlType::Named(name) if resolved.dictionaries.contains_key(name) => Some(name),
        _ => None,
      }) {
        let converted = emit_conversion_expr(resolved, &arg.type_, &arg.ext_attrs, value_ident);
        return format!(
          "if rt.is_undefined({value}) {{ js_to_dict_{dict}::<Host, R>(rt, host, {value})? }} else {{ {converted} }}",
          value = value_ident,
          dict = to_snake_ident(dict_name),
          converted = converted
        );
      }
    }
  }

  // If the argument is missing or `undefined`, use the default if present, otherwise `undefined`.
  let default_expr = arg
    .default
    .as_ref()
    .map(|lit| match lit {
      // Preserve FrozenArray distinction even when the default is `[]`.
      IdlLiteral::EmptyArray => match &arg.type_ {
        IdlType::FrozenArray(_) => "BindingValue::FrozenArray(Vec::new())".to_string(),
        IdlType::Nullable(inner) if matches!(inner.as_ref(), IdlType::FrozenArray(_)) => {
          "BindingValue::FrozenArray(Vec::new())".to_string()
        }
        _ => emit_default_literal(lit),
      },
      _ => emit_default_literal(lit),
    })
    .unwrap_or_else(|| "BindingValue::Undefined".to_string());

  let converted = emit_conversion_expr(resolved, &arg.type_, &arg.ext_attrs, value_ident);
  if type_contains_callback(resolved, &arg.type_) {
    // WebIDL callback types treat `null` similarly to `undefined` for optional arguments.
    format!(
      "if rt.is_undefined({value}) {{ {default_expr} }} else if rt.is_null({value}) {{ BindingValue::Null }} else {{ {converted} }}",
      value = value_ident,
      default_expr = default_expr,
      converted = converted,
    )
  } else {
    format!(
      "if rt.is_undefined({value}) {{ {default_expr} }} else {{ {converted} }}",
      value = value_ident,
      default_expr = default_expr,
      converted = converted,
    )
  }
}

fn type_contains_callback(resolved: &ResolvedWebIdlWorld, ty: &IdlType) -> bool {
  match ty {
    IdlType::Named(name) => {
      resolved.callbacks.contains_key(name)
        || resolved.interfaces.get(name).is_some_and(|i| i.callback)
    }
    IdlType::Nullable(inner) => type_contains_callback(resolved, inner),
    _ => false,
  }
}

fn emit_default_literal(lit: &IdlLiteral) -> String {
  match lit {
    IdlLiteral::Undefined => "BindingValue::Undefined".to_string(),
    IdlLiteral::Null => "BindingValue::Null".to_string(),
    IdlLiteral::Boolean(b) => format!("BindingValue::Bool({})", if *b { "true" } else { "false" }),
    IdlLiteral::Number(n) => {
      if let Ok(v) = n.parse::<f64>() {
        // Use debug formatting so integer-valued defaults are still emitted as float literals
        // (`0.0`), matching `BindingValue::Number(f64)` without relying on type inference.
        format!("BindingValue::Number({v:?})")
      } else {
        "BindingValue::Number(0.0)".to_string()
      }
    }
    IdlLiteral::String(s) => {
      format!(
        "BindingValue::String({}.to_string())",
        rust_string_literal(s)
      )
    }
    IdlLiteral::EmptyObject => "BindingValue::Dictionary(BTreeMap::new())".to_string(),
    IdlLiteral::EmptyArray => "BindingValue::Sequence(Vec::new())".to_string(),
    IdlLiteral::Identifier(_id) => "BindingValue::Undefined".to_string(),
  }
}

fn emit_constant_js_value_expr(lit: &IdlLiteral) -> String {
  fn parse_idl_number_literal(text: &str) -> Option<f64> {
    let s = text.trim();
    if s.eq_ignore_ascii_case("nan") {
      return Some(f64::NAN);
    }
    if s.eq_ignore_ascii_case("infinity") {
      return Some(f64::INFINITY);
    }
    if s.eq_ignore_ascii_case("-infinity") {
      return Some(f64::NEG_INFINITY);
    }

    let (sign, rest) = if let Some(rest) = s.strip_prefix('-') {
      (-1.0, rest)
    } else if let Some(rest) = s.strip_prefix('+') {
      (1.0, rest)
    } else {
      (1.0, s)
    };

    let rest = rest.trim();
    let (radix, digits) = if let Some(hex) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
      (16, hex)
    } else if let Some(oct) = rest.strip_prefix("0o").or_else(|| rest.strip_prefix("0O")) {
      (8, oct)
    } else if let Some(bin) = rest.strip_prefix("0b").or_else(|| rest.strip_prefix("0B")) {
      (2, bin)
    } else {
      // Plain decimal / exponent form.
      return rest.parse::<f64>().ok().map(|v| v * sign);
    };

    let int = u64::from_str_radix(digits.trim(), radix).ok()?;
    Some(sign * int as f64)
  }

  match lit {
    IdlLiteral::Undefined => "rt.js_undefined()".to_string(),
    IdlLiteral::Null => "rt.js_null()".to_string(),
    IdlLiteral::Boolean(b) => format!("rt.js_bool({})", if *b { "true" } else { "false" }),
    IdlLiteral::Number(n) => {
      let v = parse_idl_number_literal(n).unwrap_or(0.0);
      format!("rt.js_number({v:?})")
    }
    IdlLiteral::String(s) => format!("rt.js_string({})?", rust_string_literal(s)),
    IdlLiteral::EmptyObject | IdlLiteral::EmptyArray | IdlLiteral::Identifier(_) => "rt.js_undefined()".to_string(),
  }
}

fn emit_integer_conversion_attrs(ext_attrs: &[ExtendedAttribute]) -> String {
  let mut clamp = false;
  let mut enforce_range = false;
  for a in ext_attrs {
    match a.name.as_str() {
      "Clamp" => clamp = true,
      "EnforceRange" => enforce_range = true,
      _ => {}
    }
  }
  if !clamp && !enforce_range {
    "conversions::IntegerConversionAttrs::default()".to_string()
  } else {
    format!("conversions::IntegerConversionAttrs {{ clamp: {clamp}, enforce_range: {enforce_range} }}")
  }
}

fn emit_conversion_expr(
  resolved: &ResolvedWebIdlWorld,
  ty: &IdlType,
  ext_attrs: &[ExtendedAttribute],
  value_ident: &str,
) -> String {
  match ty {
    IdlType::Builtin(b) => match b {
      BuiltinType::Undefined => "BindingValue::Undefined".to_string(),
      BuiltinType::Any => format!("BindingValue::Object({value_ident})"),
      BuiltinType::Boolean => format!("BindingValue::Bool(rt.to_boolean({value_ident})?)"),
      BuiltinType::DOMString | BuiltinType::USVString | BuiltinType::ByteString => {
        // Avoid nested mutable borrows of `rt` by splitting `ToString` + `js_string_to_rust_string`
        // into two distinct steps.
        format!(
          "{{ let s = rt.to_string({value_ident})?; BindingValue::String(rt.js_string_to_rust_string(s)?) }}"
        )
      }
      BuiltinType::Object => format!("BindingValue::Object({value_ident})"),
      BuiltinType::Byte => format!(
        "BindingValue::Number(conversions::to_byte(rt, {value_ident}, {})? as f64)",
        emit_integer_conversion_attrs(ext_attrs)
      ),
      BuiltinType::Octet => format!(
        "BindingValue::Number(conversions::to_octet(rt, {value_ident}, {})? as f64)",
        emit_integer_conversion_attrs(ext_attrs)
      ),
      BuiltinType::Short => format!(
        "BindingValue::Number(conversions::to_short(rt, {value_ident}, {})? as f64)",
        emit_integer_conversion_attrs(ext_attrs)
      ),
      BuiltinType::UnsignedShort => format!(
        "BindingValue::Number(conversions::to_unsigned_short(rt, {value_ident}, {})? as f64)",
        emit_integer_conversion_attrs(ext_attrs)
      ),
      BuiltinType::Long => format!(
        "BindingValue::Number(conversions::to_long(rt, {value_ident}, {})? as f64)",
        emit_integer_conversion_attrs(ext_attrs)
      ),
      BuiltinType::UnsignedLong => format!(
        "BindingValue::Number(conversions::to_unsigned_long(rt, {value_ident}, {})? as f64)",
        emit_integer_conversion_attrs(ext_attrs)
      ),
      BuiltinType::LongLong => format!(
        "BindingValue::Number(conversions::to_long_long(rt, {value_ident}, {})? as f64)",
        emit_integer_conversion_attrs(ext_attrs)
      ),
      BuiltinType::UnsignedLongLong => format!(
        "BindingValue::Number(conversions::to_unsigned_long_long(rt, {value_ident}, {})? as f64)",
        emit_integer_conversion_attrs(ext_attrs)
      ),
      BuiltinType::Float => {
        format!("BindingValue::Number(conversions::to_float(rt, {value_ident})? as f64)")
      }
      BuiltinType::UnrestrictedFloat => format!(
        "BindingValue::Number(conversions::to_unrestricted_float(rt, {value_ident})? as f64)"
      ),
      BuiltinType::Double => format!("BindingValue::Number(conversions::to_double(rt, {value_ident})?)"),
      BuiltinType::UnrestrictedDouble => {
        format!("BindingValue::Number(conversions::to_unrestricted_double(rt, {value_ident})?)")
      }
    },
    IdlType::Named(name) => {
      if resolved.dictionaries.contains_key(name) {
        format!(
          "js_to_dict_{}::<Host, R>(rt, host, {value_ident})?",
          to_snake_ident(name)
        )
      } else if let Some(cb) = resolved.callbacks.get(name) {
        let legacy =
          cb.ext_attrs.iter().any(|a| a.name.as_str() == "LegacyTreatNonObjectAsNull");
        if legacy {
          format!(
            "if !rt.is_object({value_ident}) {{ BindingValue::Null }} else {{ BindingValue::Callback(rt.root_callback_function({value_ident})?) }}",
            value_ident = value_ident,
          )
        } else {
          format!("BindingValue::Callback(rt.root_callback_function({value_ident})?)")
        }
      } else if resolved.interfaces.get(name).is_some_and(|i| i.callback) {
        format!("BindingValue::Callback(rt.root_callback_interface({value_ident})?)")
      } else {
        // Fallback: treat as an opaque object/value.
        format!("BindingValue::Object({value_ident})")
      }
    }
    IdlType::Nullable(inner) => format!(
      "if rt.is_null({value_ident}) || rt.is_undefined({value_ident}) {{ BindingValue::Null }} else {{ {} }}",
      emit_conversion_expr(resolved, inner, ext_attrs, value_ident)
    ),
    IdlType::Union(_members) => {
      // Only support the common `({Dictionary} or boolean)` pattern for now (e.g.
      // `AddEventListenerOptions or boolean`).
      //
      // Spec: https://webidl.spec.whatwg.org/#es-union
      if let IdlType::Union(members) = ty {
        if members.len() == 2 {
          let mut dict: Option<&String> = None;
          let mut has_boolean = false;
          for m in members {
            match m {
              IdlType::Named(name) if resolved.dictionaries.contains_key(name) => dict = Some(name),
              IdlType::Builtin(BuiltinType::Boolean) => has_boolean = true,
              _ => {}
            }
          }
          if let (Some(dict_name), true) = (dict, has_boolean) {
            return format!(
              "{{\n  if rt.is_null({v}) || rt.is_undefined({v}) {{\n    js_to_dict_{dict}::<Host, R>(rt, host, {v})?\n  }} else if rt.is_object({v}) {{\n    js_to_dict_{dict}::<Host, R>(rt, host, {v})?\n  }} else {{\n    BindingValue::Bool(rt.to_boolean({v})?)\n  }}\n}}",
              v = value_ident,
              dict = to_snake_ident(dict_name),
            );
          }
        }
      }

      // Fallback: treat as opaque.
      format!("BindingValue::Object({value_ident})")
    }
    IdlType::Sequence(elem) => emit_iterable_list_conversion_expr(
      resolved,
      elem,
      ext_attrs,
      value_ident,
      "sequence",
      "Sequence",
    ),
    IdlType::FrozenArray(elem) => emit_iterable_list_conversion_expr(
      resolved,
      elem,
      ext_attrs,
      value_ident,
      "FrozenArray",
      "FrozenArray",
    ),
    IdlType::Promise(_) | IdlType::Record { .. } => format!("BindingValue::Object({value_ident})"),
  }
}

fn emit_iterable_list_conversion_expr(
  resolved: &ResolvedWebIdlWorld,
  elem_ty: &IdlType,
  ext_attrs: &[ExtendedAttribute],
  value_ident: &str,
  kind_label: &str,
  out_variant: &str,
) -> String {
  let elem_expr = emit_conversion_expr(resolved, elem_ty, ext_attrs, "next");
  format!(
    r#"{{
  if !rt.is_object({value_ident}) {{
    return Err(rt.throw_type_error("expected object for {kind_label}"));
  }}
  rt.with_stack_roots(&[{value_ident}], |rt| {{
    let mut iterator_record = rt.get_iterator(host, {value_ident})?;
    rt.with_stack_roots(&[iterator_record.iterator, iterator_record.next_method], |rt| {{
      let mut values: Vec<BindingValue<R::JsValue>> = Vec::new();
      while let Some(next) = rt.iterator_step_value(host, &mut iterator_record)? {{
        if values.len() >= rt.limits().max_sequence_length {{
          return Err(rt.throw_range_error("{kind_label} exceeds maximum length"));
        }}
        let converted = rt.with_stack_roots(&[next], |rt| Ok({elem_expr}))?;
        values.push(converted);
      }}
      Ok(BindingValue::{out_variant}(values))
    }})
  }})?
}}"#,
    value_ident = value_ident,
    kind_label = kind_label,
    out_variant = out_variant,
    elem_expr = elem_expr,
  )
}

fn required_arg_count(args: &[Argument]) -> usize {
  let mut required = 0usize;
  for arg in args {
    if arg.optional || arg.default.is_some() || arg.variadic {
      break;
    }
    required += 1;
  }
  required
}

fn max_arg_count(args: &[Argument]) -> Option<usize> {
  if args.last().is_some_and(|a| a.variadic) {
    None
  } else {
    Some(args.len())
  }
}

fn emit_type_predicate(resolved: &ResolvedWebIdlWorld, ty: &IdlType, value_expr: &str) -> String {
  match ty {
    IdlType::Builtin(b) => match b {
      BuiltinType::Boolean => format!("rt.is_boolean({value_expr})"),
      BuiltinType::DOMString | BuiltinType::USVString | BuiltinType::ByteString => {
        format!("rt.is_string({value_expr}) || rt.is_string_object({value_expr})")
      }
      BuiltinType::Object | BuiltinType::Any => format!("true"),
      BuiltinType::Byte
      | BuiltinType::Octet
      | BuiltinType::Short
      | BuiltinType::UnsignedShort
      | BuiltinType::Long
      | BuiltinType::UnsignedLong
      | BuiltinType::LongLong
      | BuiltinType::UnsignedLongLong
      | BuiltinType::Float
      | BuiltinType::UnrestrictedFloat
      | BuiltinType::Double
      | BuiltinType::UnrestrictedDouble => format!("rt.is_number({value_expr})"),
      BuiltinType::Undefined => format!("rt.is_undefined({value_expr})"),
    },
    IdlType::Named(name) => {
      if resolved.callbacks.contains_key(name) {
        format!("rt.is_callable({value_expr})")
      } else {
        format!("rt.is_object({value_expr})")
      }
    }
    IdlType::Nullable(inner) => format!(
      "rt.is_null({value_expr}) || ({})",
      emit_type_predicate(resolved, inner, value_expr)
    ),
    IdlType::Union(_members) => "true".to_string(),
    IdlType::Sequence(_) | IdlType::FrozenArray(_) | IdlType::Promise(_) | IdlType::Record { .. } => {
      "true".to_string()
    }
  }
}

fn write_constructor_wrapper(
  out: &mut String,
  resolved: &ResolvedWebIdlWorld,
  type_ctx: &webidl_ir::TypeContext,
  interface: &str,
  overloads: &[ArgumentList],
  _config: &WebIdlBindingsCodegenConfig,
) -> Result<()> {
  let fn_name = ctor_wrapper_fn_name(interface);
  out.push_str(&format!(
    "#[allow(dead_code)]\nfn {fn_name}<Host, R>(rt: &mut R, host: &mut Host, _this: R::JsValue, args: &[R::JsValue]) -> Result<R::JsValue, R::Error>\nwhere\n  R: crate::js::webidl::WebIdlBindingsRuntime<Host>,\n  Host: WebHostBindings<R>,\n{{\n",
  ));

  if overloads.len() == 1 {
    out.push_str(&indent_lines(
      &emit_ctor_overload_call(resolved, interface, 0, &overloads[0].arguments),
      2,
    ));
    out.push_str("}\n\n");
    return Ok(());
  }

  let overload_ir_set = build_overload_ir_constructor_set(resolved, type_ctx, interface, overloads)
    .with_context(|| format!("build overload-set IR for {interface}.constructor"))?;

  if let Err(diags) = crate::webidl::overload_ir::validate_overload_set(&overload_ir_set, resolved)
  {
    bail!(
      "WebIDL overload validation failed for {interface}.constructor:\n{}",
      format_overload_validation_failure(diags)
    );
  }

  let plan = compute_codegen_overload_dispatch_plan(&overload_ir_set, resolved);

  let display_name = format!("{interface}.constructor");
  let mut candidate_sigs = overload_ir_set
    .iter()
    .map(|o| format_overload_signature(&display_name, o))
    .collect::<Vec<_>>();
  candidate_sigs.sort();
  candidate_sigs.dedup();
  let no_match_expr = emit_no_matching_overload_expr(&display_name, &candidate_sigs, "args");

  let max_argc = plan
    .groups
    .iter()
    .map(|g| g.argument_count)
    .max()
    .unwrap_or(0);

  out.push_str(&format!(
    "  let argcount = std::cmp::min(args.len(), {max_argc});\n  match argcount {{\n",
    max_argc = max_argc
  ));

  for group in &plan.groups {
    out.push_str(&format!("    {} => {{\n", group.argument_count));

    if group.entries.len() == 1 {
      let overload_idx = group.entries[0].callable_id;
      let call = emit_ctor_overload_call(
        resolved,
        interface,
        overload_idx,
        &overloads[overload_idx].arguments,
      );
      out.push_str(&indent_lines(&call, 6));
      out.push_str("    },\n");
      continue;
    }

    let d = group.distinguishing_argument_index.with_context(|| {
      format!(
        "missing distinguishing argument index for {display_name} argcount={}",
        group.argument_count
      )
    })?;
    out.push_str(&format!("      let v = args[{d}];\n", d = d));

    let mut optional_candidate: Option<usize> = None;
    let mut nullable_dict_candidate: Option<usize> = None;
    let mut string_candidate: Option<usize> = None;
    let mut callback_candidate: Option<usize> = None;
    let mut async_sequence_candidate: Option<usize> = None;
    let mut sequence_candidate: Option<usize> = None;
    let mut object_like_candidate: Option<usize> = None;
    let mut boolean_candidate: Option<usize> = None;
    let mut numeric_candidate: Option<usize> = None;
    let mut bigint_candidate: Option<usize> = None;
    let mut symbol_candidate: Option<usize> = None;
    let mut interface_like_candidates: Vec<(usize, Vec<(String, u32)>)> = Vec::new();

    for (idx, entry) in group.entries.iter().enumerate() {
      let fp = &group.distinguishing_argument_types[idx];
      let overload_idx = entry.callable_id;

      if entry.optionality_list.get(d) == Some(&crate::webidl::overload_ir::Optionality::Optional) {
        if let Some(prev) = optional_candidate.replace(overload_idx) {
          if prev != overload_idx {
            bail!(
              "ambiguous overload dispatch for {display_name}: multiple optional overloads at distinguishing index {d} (argcount={})",
              group.argument_count
            );
          }
        }
      }

      if fast_path_matches_nullable_dictionary(fp) {
        if let Some(prev) = nullable_dict_candidate.replace(overload_idx) {
          if prev != overload_idx {
            bail!(
              "ambiguous overload dispatch for {display_name}: multiple nullable/dictionary-like overloads at distinguishing index {d} (argcount={})",
              group.argument_count
            );
          }
        }
      }

      if fast_path_matches_category(fp, webidl_ir::DistinguishabilityCategory::String) {
        if let Some(prev) = string_candidate.replace(overload_idx) {
          if prev != overload_idx {
            bail!(
              "ambiguous overload dispatch for {display_name}: multiple string overloads at distinguishing index {d} (argcount={})",
              group.argument_count
            );
          }
        }
      }

      if fast_path_matches_category(fp, webidl_ir::DistinguishabilityCategory::CallbackFunction) {
        callback_candidate = Some(overload_idx);
      }
      if fast_path_matches_category(fp, webidl_ir::DistinguishabilityCategory::AsyncSequence) {
        async_sequence_candidate = Some(overload_idx);
      }
      if fast_path_matches_category(fp, webidl_ir::DistinguishabilityCategory::SequenceLike) {
        sequence_candidate = Some(overload_idx);
      }

      if fast_path_matches_category(fp, webidl_ir::DistinguishabilityCategory::Object)
        || fast_path_matches_category(fp, webidl_ir::DistinguishabilityCategory::DictionaryLike)
      {
        object_like_candidate = Some(overload_idx);
      }

      if fast_path_matches_category(fp, webidl_ir::DistinguishabilityCategory::Boolean) {
        boolean_candidate = Some(overload_idx);
      }
      if fast_path_matches_category(fp, webidl_ir::DistinguishabilityCategory::Numeric) {
        numeric_candidate = Some(overload_idx);
      }
      if fast_path_matches_category(fp, webidl_ir::DistinguishabilityCategory::BigInt) {
        bigint_candidate = Some(overload_idx);
      }
      if fast_path_matches_category(fp, webidl_ir::DistinguishabilityCategory::Symbol) {
        symbol_candidate = Some(overload_idx);
      }

      if fast_path_matches_category(fp, webidl_ir::DistinguishabilityCategory::InterfaceLike) {
        interface_like_candidates.push((overload_idx, interface_ids_for_fast_path(fp)));
      }
    }

    let mut emit_call = |overload_idx: usize| -> String {
      emit_ctor_overload_call(
        resolved,
        interface,
        overload_idx,
        &overloads[overload_idx].arguments,
      )
    };

    let mut if_chain = String::new();

    if let Some(oidx) = optional_candidate {
      if_chain.push_str("      if rt.is_undefined(v) {\n");
      if_chain.push_str(&indent_lines(&emit_call(oidx), 8));
      if_chain.push_str("      }");
    }

    if let Some(oidx) = nullable_dict_candidate {
      if if_chain.is_empty() {
        if_chain.push_str("      if rt.is_null(v) || rt.is_undefined(v) {\n");
      } else {
        if_chain.push_str(" else if rt.is_null(v) || rt.is_undefined(v) {\n");
      }
      if_chain.push_str(&indent_lines(&emit_call(oidx), 8));
      if_chain.push_str("      }");
    }

    if let Some(oidx) = string_candidate {
      if if_chain.is_empty() {
        if_chain.push_str("      if rt.is_string(v) || rt.is_string_object(v) {\n");
      } else {
        if_chain.push_str(" else if rt.is_string(v) || rt.is_string_object(v) {\n");
      }
      if_chain.push_str(&indent_lines(&emit_call(oidx), 8));
      if_chain.push_str("      }");
    }

    for (oidx, iface_ids) in &interface_like_candidates {
      if iface_ids.is_empty() {
        continue;
      }
      let mut cond = String::new();
      for (idx, (_name, id)) in iface_ids.iter().enumerate() {
        if idx != 0 {
          cond.push_str(" || ");
        }
        cond.push_str(&format!(
          "rt.implements_interface(v, crate::js::webidl::InterfaceId(0x{hash:08x}))",
          hash = id
        ));
      }
      let cond = format!("rt.is_platform_object(v) && ({cond})");
      if if_chain.is_empty() {
        if_chain.push_str(&format!("      if {cond} {{\n"));
      } else {
        if_chain.push_str(&format!(" else if {cond} {{\n"));
      }
      if_chain.push_str(&indent_lines(&emit_call(*oidx), 8));
      if_chain.push_str("      }");
    }

    if let Some(oidx) = callback_candidate {
      if if_chain.is_empty() {
        if_chain.push_str("      if rt.is_callable(v) {\n");
      } else {
        if_chain.push_str(" else if rt.is_callable(v) {\n");
      }
      if_chain.push_str(&indent_lines(&emit_call(oidx), 8));
      if_chain.push_str("      }");
    }

    if let Some(oidx) = async_sequence_candidate {
      let cond = "rt.is_object(v) && {\n        let async_iter = rt.symbol_async_iterator()?;\n        let iter = rt.symbol_iterator()?;\n        let mut m = rt.get_method(v, async_iter)?;\n        if m.is_none() {\n          m = rt.get_method(v, iter)?;\n        }\n        m.is_some()\n      }"
        .replace('\t', "  ");
      if if_chain.is_empty() {
        if_chain.push_str(&format!("      if {cond} {{\n"));
      } else {
        if_chain.push_str(&format!(" else if {cond} {{\n"));
      }
      if_chain.push_str(&indent_lines(&emit_call(oidx), 8));
      if_chain.push_str("      }");
    }

    if let Some(oidx) = sequence_candidate {
      let cond = "rt.is_object(v) && {\n        let iter = rt.symbol_iterator()?;\n        rt.get_method(v, iter)?.is_some()\n      }"
        .replace('\t', "  ");
      if if_chain.is_empty() {
        if_chain.push_str(&format!("      if {cond} {{\n"));
      } else {
        if_chain.push_str(&format!(" else if {cond} {{\n"));
      }
      if_chain.push_str(&indent_lines(&emit_call(oidx), 8));
      if_chain.push_str("      }");
    }

    if let Some(oidx) = object_like_candidate {
      if if_chain.is_empty() {
        if_chain.push_str("      if rt.is_object(v) {\n");
      } else {
        if_chain.push_str(" else if rt.is_object(v) {\n");
      }
      if_chain.push_str(&indent_lines(&emit_call(oidx), 8));
      if_chain.push_str("      }");
    }

    if let Some(oidx) = boolean_candidate {
      if if_chain.is_empty() {
        if_chain.push_str("      if rt.is_boolean(v) {\n");
      } else {
        if_chain.push_str(" else if rt.is_boolean(v) {\n");
      }
      if_chain.push_str(&indent_lines(&emit_call(oidx), 8));
      if_chain.push_str("      }");
    }
    if let Some(oidx) = numeric_candidate {
      if if_chain.is_empty() {
        if_chain.push_str("      if rt.is_number(v) {\n");
      } else {
        if_chain.push_str(" else if rt.is_number(v) {\n");
      }
      if_chain.push_str(&indent_lines(&emit_call(oidx), 8));
      if_chain.push_str("      }");
    }
    if let Some(oidx) = bigint_candidate {
      if if_chain.is_empty() {
        if_chain.push_str("      if rt.is_bigint(v) {\n");
      } else {
        if_chain.push_str(" else if rt.is_bigint(v) {\n");
      }
      if_chain.push_str(&indent_lines(&emit_call(oidx), 8));
      if_chain.push_str("      }");
    }
    if let Some(oidx) = symbol_candidate {
      if if_chain.is_empty() {
        if_chain.push_str("      if rt.is_symbol(v) {\n");
      } else {
        if_chain.push_str(" else if rt.is_symbol(v) {\n");
      }
      if_chain.push_str(&indent_lines(&emit_call(oidx), 8));
      if_chain.push_str("      }");
    }

    let fallback_expr = if let Some(oidx) = string_candidate {
      emit_call(oidx)
    } else if let Some(oidx) = numeric_candidate {
      emit_call(oidx)
    } else if let Some(oidx) = boolean_candidate {
      emit_call(oidx)
    } else if let Some(oidx) = bigint_candidate {
      emit_call(oidx)
    } else {
      no_match_expr.clone()
    };

    if if_chain.is_empty() {
      out.push_str(&indent_lines(&fallback_expr, 6));
      out.push_str("    },\n");
      continue;
    }

    if_chain.push_str(" else {\n");
    if_chain.push_str(&indent_lines(&fallback_expr, 8));
    if_chain.push_str("      }\n");

    out.push_str(&if_chain);
    out.push('\n');
    out.push_str("    },\n");
  }

  out.push_str(&format!(
    "    _ => {no_match_expr},\n  }}\n",
    no_match_expr = no_match_expr
  ));
  out.push_str("}\n\n");
  Ok(())
}

fn emit_ctor_overload_call(
  resolved: &ResolvedWebIdlWorld,
  interface: &str,
  overload_idx: usize,
  arguments: &[Argument],
) -> String {
  let mut out = String::new();
  out.push_str("{\n");
  out.push_str("  let mut converted_args: Vec<BindingValue<R::JsValue>> = Vec::new();\n");
  for (idx, arg) in arguments.iter().enumerate() {
    if arg.variadic {
      out.push_str(&format!(
        "  let mut rest: Vec<BindingValue<R::JsValue>> = Vec::new();\n  for v in args.iter().copied().skip({idx}) {{\n    rest.push({});\n  }}\n  converted_args.push(BindingValue::Sequence(rest));\n",
        emit_conversion_expr(resolved, &arg.type_, &arg.ext_attrs, "v"),
      ));
      break;
    }

    out.push_str(&format!(
      "  let v{idx} = if args.len() > {idx} {{ args[{idx}] }} else {{ rt.js_undefined() }};\n",
      idx = idx
    ));
    let expr = emit_conversion_expr_for_optional(resolved, arguments, idx, arg, &format!("v{idx}"));
    out.push_str(&format!("  converted_args.push({expr});\n"));
  }
  out.push_str(&format!(
    "  let result = host.call_operation(rt, None, {iface_lit}, \"constructor\", {overload_idx}, converted_args)?;\n",
    iface_lit = rust_string_literal(interface),
    overload_idx = overload_idx
  ));
  out.push_str("  binding_value_to_js::<Host, R>(rt, result)\n");
  out.push_str("}\n");
  out
}

fn op_wrapper_fn_name(interface: &str, op_name: &str) -> String {
  format!("{}_{}", to_snake_ident(interface), to_snake_ident(op_name))
}

fn attr_getter_fn_name(interface: &str, attr_name: &str, is_static: bool) -> String {
  if is_static {
    format!(
      "{}_get_static_attribute_{}",
      to_snake_ident(interface),
      to_snake_ident(attr_name)
    )
  } else {
    format!(
      "{}_get_attribute_{}",
      to_snake_ident(interface),
      to_snake_ident(attr_name)
    )
  }
}

fn attr_setter_fn_name(interface: &str, attr_name: &str, is_static: bool) -> String {
  if is_static {
    format!(
      "{}_set_static_attribute_{}",
      to_snake_ident(interface),
      to_snake_ident(attr_name)
    )
  } else {
    format!(
      "{}_set_attribute_{}",
      to_snake_ident(interface),
      to_snake_ident(attr_name)
    )
  }
}

fn ctor_wrapper_fn_name(interface: &str) -> String {
  format!("{}_constructor", to_snake_ident(interface))
}

fn to_snake_ident(name: &str) -> String {
  let mut out = String::new();
  for (i, ch) in name.chars().enumerate() {
    if ch.is_ascii_uppercase() {
      if i != 0 {
        out.push('_');
      }
      out.push(ch.to_ascii_lowercase());
    } else if ch == '-' {
      out.push('_');
    } else {
      out.push(ch);
    }
  }
  if out.is_empty() {
    "_".to_string()
  } else {
    out
  }
}

fn rust_string_literal(value: &str) -> String {
  let mut out = String::with_capacity(value.len() + 2);
  out.push('"');
  for ch in value.chars() {
    out.extend(ch.escape_default());
  }
  out.push('"');
  out
}

fn indent_lines(s: &str, spaces: usize) -> String {
  let prefix = " ".repeat(spaces);
  let mut out = String::new();
  for line in s.lines() {
    if line.is_empty() {
      out.push('\n');
      continue;
    }
    out.push_str(&prefix);
    out.push_str(line);
    out.push('\n');
  }
  out
}
