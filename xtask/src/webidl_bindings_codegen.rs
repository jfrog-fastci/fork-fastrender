use anyhow::{bail, Context, Result};
use clap::Args;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use fastrender::webidl::{
  WebIdlCallback, WebIdlDictionary, WebIdlEnum, WebIdlExtendedAttribute, WebIdlInterface,
  WebIdlInterfaceMember, WebIdlInterfaceMixin, WebIdlTypedef, WebIdlWorld,
};

use crate::webidl::analyze::AnalyzedWebIdlWorld;
use crate::webidl::ast::{Argument, BuiltinType, IdlLiteral, IdlType, InterfaceMember};
use crate::webidl::resolve::{ExposureTarget, ResolvedWebIdlWorld};

#[derive(Args, Debug)]
pub struct WebIdlBindingsCodegenArgs {
  /// Output Rust module path (relative to repo root unless absolute).
  #[arg(
    long,
    default_value = "src/js/bindings/generated/mod.rs",
    value_name = "FILE"
  )]
  pub out: PathBuf,

  /// Path to the DOM-scaffold bindings allowlist manifest (TOML).
  #[arg(
    long,
    default_value = "tools/webidl/bindings_allowlist.toml",
    value_name = "FILE"
  )]
  pub dom_allowlist: PathBuf,

  /// Output Rust module path for the DOM-scaffold bindings (relative to repo root unless absolute).
  #[arg(
    long,
    default_value = "src/js/bindings/dom_generated.rs",
    value_name = "FILE"
  )]
  pub dom_out: PathBuf,

  /// Do not write files; instead, fail if the generated output differs.
  #[arg(long)]
  pub check: bool,

  /// Interface allow-list (can be passed multiple times). Defaults to a small Window-facing core
  /// subset.
  #[arg(long = "allow-interface", value_name = "NAME")]
  pub allow_interfaces: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WebIdlBindingsGenerationMode {
  /// Emit the minimal binding glue needed for early Window-facing APIs.
  CoreWindow,
  /// Emit operations/constructors for all selected interfaces (used by unit tests).
  AllMembers,
}

#[derive(Debug, Clone)]
pub struct WebIdlBindingsCodegenConfig {
  pub mode: WebIdlBindingsGenerationMode,
  pub allow_interfaces: BTreeSet<String>,
}

impl WebIdlBindingsCodegenConfig {
  pub fn core_window_default() -> Self {
    Self {
      mode: WebIdlBindingsGenerationMode::CoreWindow,
      allow_interfaces: [
        "Window",
        "Document",
        "Node",
        "Element",
        "EventTarget",
        "Event",
        "URL",
      ]
      .into_iter()
      .map(|s| s.to_string())
      .collect(),
    }
  }
}

pub fn run_webidl_bindings_codegen(args: WebIdlBindingsCodegenArgs) -> Result<()> {
  let repo_root = repo_root();
  let rustfmt_config = repo_root.join(".rustfmt.toml");

  let out_path = absolutize(repo_root.clone(), args.out);
  let dom_allowlist_path = absolutize(repo_root.clone(), args.dom_allowlist);
  let dom_out_path = absolutize(repo_root.clone(), args.dom_out);

  let allow_interfaces = if args.allow_interfaces.is_empty() {
    WebIdlBindingsCodegenConfig::core_window_default().allow_interfaces
  } else {
    args.allow_interfaces.into_iter().collect()
  };

  // Prefer the committed snapshot (`src/webidl/generated`) so running this xtask does not require
  // vendored spec submodules.
  let snapshot_world: &WebIdlWorld = &fastrender::webidl::generated::WORLD;
  let snapshot_idl = snapshot_world_to_idl(snapshot_world);

  let generated_window = generate_bindings_module_from_idl_with_config(
    &snapshot_idl,
    &rustfmt_config,
    WebIdlBindingsCodegenConfig {
      mode: WebIdlBindingsGenerationMode::CoreWindow,
      allow_interfaces,
    },
  )
  .context("generate WebIDL bindings module")?;

  let dom_allowlist_text = fs::read_to_string(&dom_allowlist_path).with_context(|| {
    format!(
      "read WebIDL DOM bindings allowlist {}",
      dom_allowlist_path.display()
    )
  })?;
  let dom_manifest: DomAllowlistManifest = toml::from_str(&dom_allowlist_text)
    .context("parse WebIDL DOM bindings allowlist TOML")?;
  let generated_dom = generate_dom_bindings_module(&dom_manifest, &rustfmt_config)
    .context("generate DOM scaffold bindings module")?;

  if args.check {
    let existing = fs::read_to_string(&out_path)
      .with_context(|| format!("read generated file {}", out_path.display()))?;
    if existing != generated_window {
      bail!(
        "generated WebIDL bindings are out of date: run `bash scripts/cargo_agent.sh xtask webidl-bindings` (path={})",
        out_path.display()
      );
    }

    let existing_dom = fs::read_to_string(&dom_out_path)
      .with_context(|| format!("read generated file {}", dom_out_path.display()))?;
    if existing_dom != generated_dom {
      bail!(
        "generated DOM bindings are out of date: run `bash scripts/cargo_agent.sh xtask webidl-bindings` (path={})",
        dom_out_path.display()
      );
    }
    return Ok(());
  }

  if let Some(parent) = out_path.parent() {
    fs::create_dir_all(parent)
      .with_context(|| format!("create output directory {}", parent.display()))?;
  }
  fs::write(&out_path, generated_window)
    .with_context(|| format!("write generated output {}", out_path.display()))?;

  if let Some(parent) = dom_out_path.parent() {
    fs::create_dir_all(parent)
      .with_context(|| format!("create output directory {}", parent.display()))?;
  }
  fs::write(&dom_out_path, generated_dom)
    .with_context(|| format!("write generated output {}", dom_out_path.display()))?;

  Ok(())
}

pub fn generate_bindings_module_from_idl_with_config(
  idl: &str,
  rustfmt_config_path: &Path,
  config: WebIdlBindingsCodegenConfig,
) -> Result<String> {
  let parsed = crate::webidl::parse_webidl(idl).context("parse WebIDL")?;
  let resolved = crate::webidl::resolve::resolve_webidl_world(&parsed);
  let resolved = resolved.filter_by_exposure(ExposureTarget::Window);
  let analyzed = crate::webidl::analyze::analyze_resolved_world(&resolved);

  let raw = generate_bindings_module_unformatted(&resolved, &analyzed, &config);
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
  out.push_str(if iface.callback { "callback interface " } else { "interface " });
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

fn dom_compute_derived_interfaces(interfaces: &[DomParsedInterface]) -> BTreeMap<String, BTreeSet<String>> {
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

  out.push_str("// @generated by `cargo xtask webidl-bindings`. DO NOT EDIT.\n");
  out.push_str("//\n");
  out.push_str("// Source inputs:\n");
  out.push_str(
    "// - src/webidl/generated/mod.rs (committed snapshot; produced by `cargo xtask webidl`)\n",
  );
  out.push_str("// - tools/webidl/bindings_allowlist.toml\n\n");
  out.push_str("use super::DomHost;\n");
  out.push_str("use crate::js::webidl::{JsRuntime, VmJsRuntime, WebIdlJsRuntime};\n");
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

fn dom_validate_boolean_dictionary_overload_set(iface: &DomParsedInterface, op_name: &str) -> Result<()> {
  use webidl_ir::{IdlType, NamedType, NamedTypeKind, StringType};
  use crate::webidl::overload_ir::{
    validate_overload_set, Optionality, Overload, OverloadArgument, WorldContext,
  };

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
}

#[derive(Debug, Clone)]
struct OperationSig {
  name: String,
  return_type: IdlType,
  arguments: Vec<Argument>,
}

#[derive(Debug, Clone)]
struct ArgumentList {
  arguments: Vec<Argument>,
}

fn generate_bindings_module_unformatted(
  resolved: &ResolvedWebIdlWorld,
  analyzed: &AnalyzedWebIdlWorld,
  config: &WebIdlBindingsCodegenConfig,
) -> String {
  let selected = select_interfaces(analyzed, config);
  let referenced_dicts = collect_referenced_dictionaries(resolved, &selected);

  let mut out = String::new();

  out.push_str("// @generated by `cargo xtask webidl-bindings`. DO NOT EDIT.\n");
  out.push_str("//\n");
  out.push_str("// Source inputs:\n");
  out.push_str(
    "// - src/webidl/generated/mod.rs (committed snapshot; produced by `cargo xtask webidl`)\n",
  );
  out.push_str("\n");

  out.push_str("use std::collections::BTreeMap;\n\n");
  out.push_str("use super::host::{BindingValue, WebHostBindings};\n\n");

  out.push_str("fn binding_value_to_js<Host, R>(\n");
  out.push_str("  rt: &mut R,\n");
  out.push_str("  value: BindingValue<R::JsValue>,\n");
  out.push_str(") -> Result<R::JsValue, R::Error>\n");
  out.push_str("where\n");
  out.push_str("  R: webidl_js_runtime::WebIdlBindingsRuntime<Host>,\n");
  out.push_str("{\n");
  out.push_str("  match value {\n");
  out.push_str("    BindingValue::Undefined => Ok(rt.js_undefined()),\n");
  out.push_str("    BindingValue::Null => Ok(rt.js_null()),\n");
  out.push_str("    BindingValue::Bool(b) => Ok(rt.js_bool(b)),\n");
  out.push_str("    BindingValue::Number(n) => Ok(rt.js_number(n)),\n");
  out.push_str("    BindingValue::String(s) => rt.js_string(&s),\n");
  out.push_str("    BindingValue::Object(v) => Ok(v),\n");
  out.push_str("    BindingValue::Sequence(values) => {\n");
  out.push_str("      let obj = rt.create_object()?;\n");
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
    if let Some(dict) = resolved.dictionaries.get(dict_name) {
      write_dictionary_converter(&mut out, resolved, dict);
    }
  }

  // Operation shims.
  for iface in selected.values() {
    for (op_name, overloads) in &iface.operations {
      write_operation_wrapper(
        &mut out,
        resolved,
        &iface.name,
        op_name,
        overloads,
        false,
        config,
      );
    }
    for (op_name, overloads) in &iface.static_operations {
      write_operation_wrapper(
        &mut out,
        resolved,
        &iface.name,
        op_name,
        overloads,
        true,
        config,
      );
    }
    if !iface.constructors.is_empty() {
      write_constructor_wrapper(&mut out, resolved, &iface.name, &iface.constructors, config);
    }
  }

  // Install entrypoint.
  out.push_str("pub fn install_window_bindings<Host, R>(rt: &mut R, host: &mut Host) -> Result<(), R::Error>\n");
  out.push_str("where\n");
  out.push_str("  R: webidl_js_runtime::WebIdlBindingsRuntime<Host>,\n");
  out.push_str("  Host: WebHostBindings<R>,\n");
  out.push_str("{\n");
  out.push_str("  let global = rt.global_object()?;\n");

  // Create prototypes.
  for iface_name in selected.keys() {
    let iface = &selected[iface_name];
    if iface.name == "Window" {
      continue;
    }
    out.push_str(&format!(
      "  let proto_{snake} = rt.create_object()?;\n",
      snake = to_snake_ident(&iface.name)
    ));
  }

  // Set prototype chains.
  for iface_name in selected.keys() {
    let iface = &selected[iface_name];
    if iface.name == "Window" {
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

  // Define constructors + prototypes + methods.
  for iface_name in selected.keys() {
    let iface = &selected[iface_name];
    if iface.name == "Window" {
      // Global functions live on the global object.
      for op_name in iface.operations.keys() {
        out.push_str(&format!(
          "  let func = rt.create_function({func}::<Host, R>)?;\n  rt.define_data_property_str(global, \"{name}\", func, true)?;\n",
          name = op_name,
          func = op_wrapper_fn_name(&iface.name, op_name)
        ));
      }
      continue;
    }

    let proto_var = format!("proto_{}", to_snake_ident(&iface.name));

    // Prototype methods.
    for op_name in iface.operations.keys() {
      out.push_str(&format!(
        "  let func = rt.create_function({func}::<Host, R>)?;\n  rt.define_data_property_str({proto}, \"{name}\", func, true)?;\n",
        proto = proto_var.as_str(),
        name = op_name,
        func = op_wrapper_fn_name(&iface.name, op_name)
      ));
    }

    if iface.constructors.is_empty() && iface.static_operations.is_empty() {
      continue;
    }

    // Constructor function (even for static-only interfaces like URL).
    let ctor_fn = ctor_wrapper_fn_name(&iface.name);
    out.push_str(&format!(
      "  let ctor_{snake} = rt.create_function({ctor_fn}::<Host, R>)?;\n",
      snake = to_snake_ident(&iface.name),
      ctor_fn = ctor_fn
    ));
    out.push_str(&format!(
      "  rt.define_data_property_str(global, \"{name}\", ctor_{snake}, true)?;\n",
      name = iface.name.as_str(),
      snake = to_snake_ident(&iface.name)
    ));
    out.push_str(&format!(
      "  rt.define_data_property_str(ctor_{snake}, \"prototype\", {proto}, false)?;\n",
      snake = to_snake_ident(&iface.name),
      proto = proto_var.as_str()
    ));
    out.push_str(&format!(
      "  rt.define_data_property_str({proto}, \"constructor\", ctor_{snake}, false)?;\n",
      proto = proto_var.as_str(),
      snake = to_snake_ident(&iface.name)
    ));

    // Static methods.
    for op_name in iface.static_operations.keys() {
      out.push_str(&format!(
        "  let func = rt.create_function({func}::<Host, R>)?;\n  rt.define_data_property_str(ctor_{snake}, \"{name}\", func, true)?;\n",
        snake = to_snake_ident(&iface.name),
        name = op_name,
        func = op_wrapper_fn_name(&iface.name, op_name)
      ));
    }
  }

  out.push_str("  let _ = host;\n");
  out.push_str("  Ok(())\n");
  out.push_str("}\n");

  out
}

fn select_interfaces(analyzed: &AnalyzedWebIdlWorld, config: &WebIdlBindingsCodegenConfig) -> BTreeMap<String, SelectedInterface> {
  let mut out = BTreeMap::<String, SelectedInterface>::new();

  for iface_name in &config.allow_interfaces {
    let Some(iface) = analyzed.interfaces.get(iface_name) else {
      continue;
    };

    let mut constructors: Vec<ArgumentList> = Vec::new();
    let mut operations: BTreeMap<String, Vec<OperationSig>> = BTreeMap::new();
    let mut static_operations: BTreeMap<String, Vec<OperationSig>> = BTreeMap::new();

    for member in &iface.members {
      match &member.parsed {
        InterfaceMember::Constructor { arguments } => {
          if should_emit_member(config.mode, iface.name.as_str(), "constructor") {
            constructors.push(ArgumentList {
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
          if should_emit_member(config.mode, iface.name.as_str(), op_name) {
            let sig = OperationSig {
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
        _ => {}
      }
    }

    if constructors.is_empty() && operations.is_empty() && static_operations.is_empty() {
      continue;
    }

    out.insert(
      iface.name.clone(),
      SelectedInterface {
        name: iface.name.clone(),
        inherits: iface.inherits.clone(),
        constructors,
        operations,
        static_operations,
      },
    );
  }

  out
}

fn should_emit_member(mode: WebIdlBindingsGenerationMode, iface: &str, member_name: &str) -> bool {
  match mode {
    WebIdlBindingsGenerationMode::AllMembers => true,
    WebIdlBindingsGenerationMode::CoreWindow => match iface {
      "EventTarget" => matches!(
        member_name,
        "addEventListener" | "removeEventListener" | "dispatchEvent" | "constructor"
      ),
      "URL" => true,
      "Window" => matches!(
        member_name,
        "setTimeout" | "setInterval" | "clearTimeout" | "clearInterval" | "queueMicrotask"
      ),
      _ => false,
    },
  }
}

fn collect_referenced_dictionaries(resolved: &ResolvedWebIdlWorld, interfaces: &BTreeMap<String, SelectedInterface>) -> BTreeSet<String> {
  let mut referenced = BTreeSet::<String>::new();

  let mut queue = Vec::<IdlType>::new();
  for iface in interfaces.values() {
    for ctor in &iface.constructors {
      for arg in &ctor.arguments {
        queue.push(arg.type_.clone());
      }
    }
    for overloads in iface.operations.values().chain(iface.static_operations.values()) {
      for sig in overloads {
        queue.push(sig.return_type.clone());
        for arg in &sig.arguments {
          queue.push(arg.type_.clone());
        }
      }
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
      let members = resolved.flattened_dictionary_members(&name);
      for member in members {
        if let Some((ty, _member_name)) = parse_dictionary_member_type(&member.raw) {
          let mut names = BTreeSet::new();
          collect_named_types(&ty, &mut names);
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
      let td = &resolved.typedefs[&name];
      if let Ok(ty) = crate::webidl::parse_idl_type(&td.type_) {
        let mut names = BTreeSet::new();
        collect_named_types(&ty, &mut names);
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

fn write_dictionary_converter(
  out: &mut String,
  resolved: &ResolvedWebIdlWorld,
  dict: &crate::webidl::resolve::ResolvedDictionary,
) {
  let fn_name = format!("js_to_dict_{}", to_snake_ident(&dict.name));
  out.push_str(&format!(
    "#[allow(dead_code)]\nfn {fn_name}<Host, R>(rt: &mut R, value: R::JsValue) -> Result<BindingValue<R::JsValue>, R::Error>\nwhere\n  R: webidl_js_runtime::WebIdlBindingsRuntime<Host>,\n{{\n",
  ));
  out.push_str("  if rt.is_undefined(value) || rt.is_null(value) {\n");
  out.push_str("    return Ok(BindingValue::Dictionary(BTreeMap::new()));\n");
  out.push_str("  }\n");
  out.push_str("  if !rt.is_object(value) {\n");
  out.push_str(&format!(
    "    return Err(rt.throw_type_error(\"expected object for dictionary {}\"));\n",
    dict.name
  ));
  out.push_str("  }\n");
  out.push_str("  let mut out_dict: BTreeMap<String, BindingValue<R::JsValue>> = BTreeMap::new();\n");

  for member in resolved.flattened_dictionary_members(&dict.name) {
    let Some((ty, member_name)) = parse_dictionary_member_type(&member.raw) else {
      continue;
    };
    out.push_str(&format!(
      "  {{\n    let key = rt.property_key({name_lit})?;\n    let v = rt.get(value, key)?;\n    if !rt.is_undefined(v) {{\n",
      name_lit = rust_string_literal(&member_name)
    ));
    out.push_str(&format!(
      "      let converted = {};\n",
      emit_conversion_expr(resolved, &ty, "v")
    ));
    out.push_str(&format!(
      "      out_dict.insert({name_lit}.to_string(), converted);\n",
      name_lit = rust_string_literal(&member_name)
    ));
    out.push_str("    }\n  }\n");
  }

  out.push_str("  Ok(BindingValue::Dictionary(out_dict))\n");
  out.push_str("}\n\n");
}

fn parse_dictionary_member_type(raw: &str) -> Option<(IdlType, String)> {
  let mut s = raw.trim();
  s = s.strip_suffix(';').unwrap_or(s).trim();

  // Strip leading `required`.
  if let Some(rest) = s.strip_prefix("required") {
    s = rest.trim_start();
  }

  // Split default.
  if let Some((before, _after)) = s.split_once('=') {
    s = before.trim_end();
  }

  // Split trailing identifier (member name).
  let mut end = s.len();
  while end > 0 && s.as_bytes()[end - 1].is_ascii_whitespace() {
    end -= 1;
  }
  let mut start = end;
  while start > 0 {
    let b = s.as_bytes()[start - 1];
    if !(b.is_ascii_alphanumeric() || b == b'_') {
      break;
    }
    start -= 1;
  }
  if start == end {
    return None;
  }
  let name = s[start..end].to_string();
  let ty_str = s[..start].trim_end();
  let ty = crate::webidl::parse_idl_type(ty_str).ok()?;
  Some((ty, name))
}

fn write_operation_wrapper(
  out: &mut String,
  resolved: &ResolvedWebIdlWorld,
  interface: &str,
  op_name: &str,
  overloads: &[OperationSig],
  is_static: bool,
  config: &WebIdlBindingsCodegenConfig,
) {
  let _ = config;
  let fn_name = op_wrapper_fn_name(interface, op_name);
  out.push_str(&format!(
    "#[allow(dead_code)]\nfn {fn_name}<Host, R>(rt: &mut R, host: &mut Host, this: R::JsValue, args: &[R::JsValue]) -> Result<R::JsValue, R::Error>\nwhere\n  R: webidl_js_runtime::WebIdlBindingsRuntime<Host>,\n  Host: WebHostBindings<R>,\n{{\n",
  ));

  let receiver_expr = if interface == "Window" || is_static {
    "None"
  } else {
    "Some(this)"
  };

  if overloads.len() == 1 {
    out.push_str(&emit_overload_call(
      resolved,
      interface,
      op_name,
      receiver_expr,
      0,
      &overloads[0].arguments,
    ));
    out.push_str("}\n\n");
    return;
  }

  // Naive overload resolution: bucket by argument count constraints, then discriminate by the first
  // differing argument's runtime type predicate.
  for (idx, sig) in overloads.iter().enumerate() {
    let cond = emit_overload_condition(sig, "args");
    if idx == 0 {
      out.push_str(&format!("  if {cond} {{\n"));
    } else {
      out.push_str(&format!("  }} else if {cond} {{\n"));
    }
    out.push_str(&indent_lines(
      &emit_overload_call(
        resolved,
        interface,
        op_name,
        receiver_expr,
        idx,
        &sig.arguments,
      ),
      2,
    ));
  }
  out.push_str("  } else {\n");
  out.push_str(&format!(
    "    Err(rt.throw_type_error(\"no matching overload for {}.{}\"))\n",
    interface, op_name
  ));
  out.push_str("  }\n");
  out.push_str("}\n\n");
}

fn emit_overload_condition(sig: &OperationSig, args_ident: &str) -> String {
  let required = required_arg_count(&sig.arguments);
  let max = max_arg_count(&sig.arguments);
  let len_check = match max {
    Some(max) => format!("{args_ident}.len() >= {required} && {args_ident}.len() <= {max}"),
    None => format!("{args_ident}.len() >= {required}"),
  };

  // If there are multiple overloads, we use the first argument's predicate as a best-effort
  // discriminator (works for the MVP overload shapes we care about).
  if sig.arguments.is_empty() {
    return len_check;
  }

  let pred = emit_type_predicate(&sig.arguments[0].type_, &format!("{args_ident}[0]"));
  if required == 0 {
    format!("{len_check} && ({args_ident}.len() == 0 || ({pred}))")
  } else {
    format!("{len_check} && ({pred})")
  }
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
  out.push_str("  {\n");
  out.push_str("    let mut converted_args: Vec<BindingValue<R::JsValue>> = Vec::new();\n");
  for (idx, arg) in arguments.iter().enumerate() {
    if arg.variadic {
      out.push_str(&format!(
        "    let mut rest: Vec<BindingValue<R::JsValue>> = Vec::new();\n    for v in args.iter().copied().skip({idx}) {{\n      rest.push({});\n    }}\n    converted_args.push(BindingValue::Sequence(rest));\n",
        emit_conversion_expr(resolved, &arg.type_, "v"),
      ));
      break;
    }

    out.push_str(&format!(
      "    let v{idx} = if args.len() > {idx} {{ args[{idx}] }} else {{ rt.js_undefined() }};\n",
      idx = idx
    ));
    let expr = emit_conversion_expr_for_optional(
      resolved,
      arguments,
      idx,
      arg,
      &format!("v{idx}"),
    );
    out.push_str(&format!("    converted_args.push({expr});\n"));
  }
  out.push_str(&format!(
    "    let result = host.call_operation(rt, {receiver_expr}, {iface_lit}, {op_lit}, {overload_idx}, converted_args)?;\n",
    receiver_expr = receiver_expr,
    iface_lit = rust_string_literal(interface),
    op_lit = rust_string_literal(operation),
    overload_idx = overload_idx
  ));
  out.push_str("    binding_value_to_js::<Host, R>(rt, result)\n");
  out.push_str("  }\n");
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
    return emit_conversion_expr(resolved, &arg.type_, value_ident);
  }

  // If the argument is missing or `undefined`, use the default if present, otherwise `undefined`.
  let default_expr = arg
    .default
    .as_ref()
    .map(|lit| emit_default_literal(lit))
    .unwrap_or_else(|| "BindingValue::Undefined".to_string());

  format!(
    "if rt.is_undefined({value}) {{ {default_expr} }} else {{ {converted} }}",
    value = value_ident,
    default_expr = default_expr,
    converted = emit_conversion_expr(resolved, &arg.type_, value_ident),
  )
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
    IdlLiteral::String(s) => format!("BindingValue::String({})", rust_string_literal(s)),
    IdlLiteral::EmptyObject => "BindingValue::Dictionary(BTreeMap::new())".to_string(),
    IdlLiteral::EmptyArray => "BindingValue::Sequence(Vec::new())".to_string(),
    IdlLiteral::Identifier(_id) => "BindingValue::Undefined".to_string(),
  }
}

fn emit_conversion_expr(resolved: &ResolvedWebIdlWorld, ty: &IdlType, value_ident: &str) -> String {
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
      | BuiltinType::UnrestrictedDouble => format!("BindingValue::Number(rt.to_number({value_ident})?)"),
    },
    IdlType::Named(name) => {
      if resolved.dictionaries.contains_key(name) {
        format!("js_to_dict_{}::<Host, R>(rt, {value_ident})?", to_snake_ident(name))
      } else {
        // Fallback: treat as an opaque object/value.
        format!("BindingValue::Object({value_ident})")
      }
    }
    IdlType::Nullable(inner) => format!(
      "if rt.is_null({value_ident}) {{ BindingValue::Null }} else {{ {} }}",
      emit_conversion_expr(resolved, inner, value_ident)
    ),
    IdlType::Union(_members) => {
      // Union conversion is non-trivial; for MVP treat as opaque.
      format!("BindingValue::Object({value_ident})")
    }
    IdlType::Sequence(_)
    | IdlType::FrozenArray(_)
    | IdlType::Promise(_)
    | IdlType::Record { .. } => format!("BindingValue::Object({value_ident})"),
  }
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

fn emit_type_predicate(ty: &IdlType, value_expr: &str) -> String {
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
    IdlType::Named(_name) => format!("rt.is_object({value_expr})"),
    IdlType::Nullable(inner) => format!("rt.is_null({value_expr}) || ({})", emit_type_predicate(inner, value_expr)),
    IdlType::Union(_members) => "true".to_string(),
    IdlType::Sequence(_) | IdlType::FrozenArray(_) | IdlType::Promise(_) | IdlType::Record { .. } => {
      "true".to_string()
    }
  }
}

fn write_constructor_wrapper(
  out: &mut String,
  resolved: &ResolvedWebIdlWorld,
  interface: &str,
  overloads: &[ArgumentList],
  _config: &WebIdlBindingsCodegenConfig,
) {
  let fn_name = ctor_wrapper_fn_name(interface);
  out.push_str(&format!(
    "#[allow(dead_code)]\nfn {fn_name}<Host, R>(rt: &mut R, host: &mut Host, _this: R::JsValue, args: &[R::JsValue]) -> Result<R::JsValue, R::Error>\nwhere\n  R: webidl_js_runtime::WebIdlBindingsRuntime<Host>,\n  Host: WebHostBindings<R>,\n{{\n",
  ));

  if overloads.len() == 1 {
    out.push_str(&emit_ctor_overload_call(
      resolved,
      interface,
      0,
      &overloads[0].arguments,
    ));
    out.push_str("}\n\n");
    return;
  }

  for (idx, sig) in overloads.iter().enumerate() {
    let required = required_arg_count(&sig.arguments);
    let max = max_arg_count(&sig.arguments);
    let cond = match max {
      Some(max) => format!("args.len() >= {required} && args.len() <= {max}"),
      None => format!("args.len() >= {required}"),
    };
    if idx == 0 {
      out.push_str(&format!("  if {cond} {{\n"));
    } else {
      out.push_str(&format!("  }} else if {cond} {{\n"));
    }
    out.push_str(&indent_lines(
      &emit_ctor_overload_call(resolved, interface, idx, &sig.arguments),
      2,
    ));
  }
  out.push_str("  } else {\n");
  out.push_str(&format!(
    "    Err(rt.throw_type_error(\"no matching overload for {} constructor\"))\n",
    interface
  ));
  out.push_str("  }\n");
  out.push_str("}\n\n");
}

fn emit_ctor_overload_call(
  resolved: &ResolvedWebIdlWorld,
  interface: &str,
  overload_idx: usize,
  arguments: &[Argument],
) -> String {
  let mut out = String::new();
  out.push_str("  {\n");
  out.push_str("    let mut converted_args: Vec<BindingValue<R::JsValue>> = Vec::new();\n");
  for (idx, arg) in arguments.iter().enumerate() {
    if arg.variadic {
      out.push_str(&format!(
        "    let mut rest: Vec<BindingValue<R::JsValue>> = Vec::new();\n    for v in args.iter().copied().skip({idx}) {{\n      rest.push({});\n    }}\n    converted_args.push(BindingValue::Sequence(rest));\n",
        emit_conversion_expr(resolved, &arg.type_, "v"),
      ));
      break;
    }

    out.push_str(&format!(
      "    let v{idx} = if args.len() > {idx} {{ args[{idx}] }} else {{ rt.js_undefined() }};\n",
      idx = idx
    ));
    let expr = emit_conversion_expr_for_optional(
      resolved,
      arguments,
      idx,
      arg,
      &format!("v{idx}"),
    );
    out.push_str(&format!("    converted_args.push({expr});\n"));
  }
  out.push_str(&format!(
    "    let result = host.call_operation(rt, None, {iface_lit}, \"constructor\", {overload_idx}, converted_args)?;\n",
    iface_lit = rust_string_literal(interface),
    overload_idx = overload_idx
  ));
  out.push_str("    binding_value_to_js::<Host, R>(rt, result)\n");
  out.push_str("  }\n");
  out
}

fn op_wrapper_fn_name(interface: &str, op_name: &str) -> String {
  format!("{}_{}", to_snake_ident(interface), to_snake_ident(op_name))
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
