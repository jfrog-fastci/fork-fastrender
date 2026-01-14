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
use crate::webidl::ast::{Argument, BuiltinType, IdlLiteral, IdlType, InterfaceMember};
use crate::webidl::resolve::{ExposureTarget, ResolvedInterface, ResolvedWebIdlWorld};
use crate::webidl::type_resolution;
use crate::webidl::type_resolution::{build_type_context, expand_typedefs_in_type};
use crate::webidl::ExtendedAttribute;
use webidl::ir::{
  DefaultValue as IrDefaultValue, IdlType as IrIdlType, NamedTypeKind,
  NumericType as IrNumericType, TypeAnnotation as IrTypeAnnotation, TypeContext,
};

#[derive(Args, Debug)]
pub struct WebIdlBindingsCodegenArgs {
  /// Codegen backend.
  ///
  /// - `vmjs` (default): emit `vm-js` realm-based WebIDL bindings.
  /// - `legacy`: emit the legacy `webidl-js-runtime` bindings (and the deprecated DOM scaffold).
  ///
  /// Note: `realm` is accepted as an alias for `vmjs` while downstream code migrates.
  #[arg(long, default_value = "vmjs", value_enum)]
  pub backend: WebIdlBindingsBackend,

  /// Output Rust module path (relative to repo root unless absolute).
  ///
  /// Defaults to:
  /// - `src/js/webidl/bindings/generated/mod.rs` for `--backend vmjs` (default)
  /// - `src/js/webidl/bindings/generated_legacy.rs` for `--backend legacy`
  #[arg(long, value_name = "FILE")]
  pub out: Option<PathBuf>,

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
  /// Also generate the deprecated `VmJsRuntime` DOM scaffold.
  Legacy,
  /// `vm-js` realm-based bindings.
  #[value(alias = "realm")]
  Vmjs,
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

  let WebIdlBindingsCodegenArgs {
    backend,
    out,
    window_allowlist,
    dom_allowlist,
    dom_out,
    check,
    exposure_target,
    allow_interfaces,
  } = args;

  let out_path = absolutize(
    repo_root.clone(),
    out.unwrap_or_else(|| match backend {
      WebIdlBindingsBackend::Vmjs => PathBuf::from("src/js/webidl/bindings/generated/mod.rs"),
      WebIdlBindingsBackend::Legacy => PathBuf::from("src/js/webidl/bindings/generated_legacy.rs"),
    }),
  );
  let window_allowlist_path = absolutize(repo_root.clone(), window_allowlist);
  let dom_allowlist_path = absolutize(repo_root.clone(), dom_allowlist);
  let dom_out_path = absolutize(repo_root.clone(), dom_out);

  let cmd_for_regen = || {
    let out = out_path
      .strip_prefix(&repo_root)
      .map(|p| p.display().to_string())
      .unwrap_or_else(|_| out_path.display().to_string());
    let dom_out = dom_out_path
      .strip_prefix(&repo_root)
      .map(|p| p.display().to_string())
      .unwrap_or_else(|_| dom_out_path.display().to_string());

    match backend {
      WebIdlBindingsBackend::Vmjs => {
        format!("bash scripts/cargo_agent.sh xtask webidl-bindings --out {out}")
      }
      WebIdlBindingsBackend::Legacy => format!(
        "bash scripts/cargo_agent.sh xtask webidl-bindings --backend legacy --out {out} --dom-out {dom_out}"
      ),
    }
  };

  // Prefer the committed snapshot (`src/webidl/generated`) so running this xtask does not require
  // vendored spec submodules.
  let snapshot_world: &WebIdlWorld = &fastrender::webidl::generated::WORLD;
  let mut snapshot_idl = snapshot_world_to_idl(snapshot_world);
  // Append project-local WebIDL definitions that are intentionally *not* part of the committed
  // upstream snapshot (`src/webidl/generated`). This keeps the snapshot spec-shaped while still
  // allowing our custom, Window-exposed bridge APIs (e.g. browser chrome JS ↔ host dispatch) to use
  // the same WebIDL argument conversion + overload resolution infrastructure as standard APIs.
  let local_chrome_idl_path = repo_root.join("tools/webidl/local/chrome.idl");
  let local_chrome_idl = fs::read_to_string(&local_chrome_idl_path)
    .with_context(|| format!("read local WebIDL {}", local_chrome_idl_path.display()))?;
  snapshot_idl.push_str("\n\n");
  snapshot_idl.push_str(&local_chrome_idl);

  let window_config = if allow_interfaces.is_empty() {
    let allowlist_text = fs::read_to_string(&window_allowlist_path).with_context(|| {
      format!(
        "read WebIDL Window bindings allowlist {}",
        window_allowlist_path.display()
      )
    })?;
    let manifest: WindowBindingsAllowlistManifest =
      toml::from_str(&allowlist_text).context("parse WebIDL Window bindings allowlist TOML")?;
    // Parse+resolve the same combined IDL input that will be used for codegen so allowlist entries
    // can reference local additions (e.g. `FastRenderChrome`) while still catching typos.
    let parsed = crate::webidl::parse_webidl(&snapshot_idl).context("parse WebIDL")?;
    let resolved = crate::webidl::resolve::resolve_webidl_world(&parsed);
    let interface_allowlist = window_parse_allowlisted_interfaces(&resolved, &manifest.interfaces)?;
    WebIdlBindingsCodegenConfig {
      mode: WebIdlBindingsGenerationMode::Allowlist,
      allow_interfaces: interface_allowlist.keys().cloned().collect(),
      interface_allowlist,
      prototype_chains: manifest.prototype_chains,
    }
  } else {
    WebIdlBindingsCodegenConfig {
      mode: WebIdlBindingsGenerationMode::AllMembers,
      allow_interfaces: allow_interfaces.into_iter().collect(),
      interface_allowlist: BTreeMap::new(),
      prototype_chains: true,
    }
  };

  let generated_bindings = generate_bindings_module_from_idl_with_config(
    &snapshot_idl,
    &rustfmt_config,
    exposure_target,
    window_config,
    backend,
  )
  .context("generate WebIDL bindings module")?;

  let generated_dom = if backend == WebIdlBindingsBackend::Legacy {
    let dom_allowlist_text = fs::read_to_string(&dom_allowlist_path).with_context(|| {
      format!(
        "read WebIDL DOM bindings allowlist {}",
        dom_allowlist_path.display()
      )
    })?;
    let dom_manifest: DomAllowlistManifest =
      toml::from_str(&dom_allowlist_text).context("parse WebIDL DOM bindings allowlist TOML")?;
    Some(
      generate_dom_bindings_module(&dom_manifest, &rustfmt_config)
        .context("generate DOM scaffold bindings module")?,
    )
  } else {
    None
  };

  if check {
    let existing = fs::read_to_string(&out_path)
      .with_context(|| format!("read generated file {}", out_path.display()))?;
    if existing != generated_bindings {
      bail!(
        "generated WebIDL bindings are out of date: run `{}` (path={})",
        cmd_for_regen(),
        out_path.display()
      );
    }

    if let Some(generated_dom) = generated_dom.as_ref() {
      let existing_dom = fs::read_to_string(&dom_out_path)
        .with_context(|| format!("read generated file {}", dom_out_path.display()))?;
      if existing_dom != *generated_dom {
        bail!(
          "generated DOM bindings are out of date: run `{}` (path={})",
          cmd_for_regen(),
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
  backend: WebIdlBindingsBackend,
) -> Result<String> {
  let parsed = crate::webidl::parse_webidl(idl).context("parse WebIDL")?;
  let resolved = crate::webidl::resolve::resolve_webidl_world(&parsed);
  let raw = match backend {
    WebIdlBindingsBackend::Legacy => {
      generate_bindings_module_unformatted(&resolved, exposure_target, &config)?
    }
    WebIdlBindingsBackend::Vmjs => {
      generate_bindings_module_unformatted_vmjs(&resolved, exposure_target, &config)?
    }
  };
  let formatted = crate::webidl::generate::rustfmt(&raw, rustfmt_config_path)?;
  crate::webidl::generate::ensure_no_forbidden_tokens(&formatted)?;
  ensure_no_duplicate_rust_fns_in_bindings_modules(&formatted)?;
  if backend == WebIdlBindingsBackend::Legacy {
    ensure_all_runtime_callbacks_defined_in_bindings_modules(&formatted)?;
  }
  Ok(formatted)
}

fn ensure_no_duplicate_rust_fns_in_bindings_modules(src: &str) -> Result<()> {
  // These bindings are generated as a single Rust file with `pub mod window { ... }` and
  // `pub mod worker { ... }` inline modules. Duplicate `fn` definitions within a module are a hard
  // compilation error (E0428), so detect them during codegen to make failures actionable.
  let mut modules: Vec<(&str, usize)> = Vec::new();
  for name in ["window", "worker"] {
    if let Some(idx) = src.find(&format!("pub mod {name} {{")) {
      modules.push((name, idx));
    }
  }
  if modules.is_empty() {
    return Ok(());
  }
  modules.sort_by_key(|(_, idx)| *idx);

  for (idx, (name, start)) in modules.iter().copied().enumerate() {
    let end = modules
      .get(idx + 1)
      .map(|(_, next_start)| *next_start)
      .unwrap_or(src.len());
    ensure_no_duplicate_rust_fns_in_single_module(name, &src[start..end])?;
  }

  Ok(())
}

fn ensure_no_duplicate_rust_fns_in_single_module(module_name: &str, src: &str) -> Result<()> {
  let mut counts: BTreeMap<String, usize> = BTreeMap::new();

  for line in src.lines() {
    let trimmed = line.trim_start();
    let rest = trimmed
      .strip_prefix("pub fn ")
      .or_else(|| trimmed.strip_prefix("fn "));
    let Some(rest) = rest else {
      continue;
    };
    let name = rest
      .chars()
      .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
      .collect::<String>();
    if name.is_empty() {
      continue;
    }
    *counts.entry(name).or_insert(0) += 1;
  }

  let duplicates: Vec<String> = counts
    .into_iter()
    .filter_map(|(name, count)| (count > 1).then(|| format!("{name} ({count}x)")))
    .collect();

  if duplicates.is_empty() {
    return Ok(());
  }

  bail!(
    "generated WebIDL bindings contain duplicate `fn` definitions in module `{module_name}`: {}",
    duplicates.join(", ")
  );
}

fn ensure_all_runtime_callbacks_defined_in_bindings_modules(src: &str) -> Result<()> {
  // The legacy bindings backend uses `webidl-js-runtime` which requires passing Rust function items
  // to `rt.create_function`/`rt.create_constructor`. If codegen installs an accessor or method but
  // forgets to emit the corresponding wrapper function, compilation fails with an unresolved name
  // error.
  //
  // Keep this check lightweight (string-based) so it can run as part of `xtask webidl-bindings
  // --check`, providing an actionable generator error before the broken output lands in-tree.
  let mut modules: Vec<(&str, usize)> = Vec::new();
  for name in ["window", "worker"] {
    if let Some(idx) = src.find(&format!("pub mod {name} {{")) {
      modules.push((name, idx));
    }
  }
  if modules.is_empty() {
    return Ok(());
  }
  modules.sort_by_key(|(_, idx)| *idx);

  for (idx, (name, start)) in modules.iter().copied().enumerate() {
    let end = modules
      .get(idx + 1)
      .map(|(_, next_start)| *next_start)
      .unwrap_or(src.len());
    ensure_all_runtime_callbacks_defined_in_single_module(name, &src[start..end])?;
  }

  Ok(())
}

fn ensure_all_runtime_callbacks_defined_in_single_module(
  module_name: &str,
  src: &str,
) -> Result<()> {
  use std::collections::BTreeSet;

  // 1) Collect all function definitions in this module.
  let mut defined: BTreeSet<String> = BTreeSet::new();
  for line in src.lines() {
    let trimmed = line.trim_start();
    let rest = trimmed
      .strip_prefix("pub fn ")
      .or_else(|| trimmed.strip_prefix("fn "));
    let Some(rest) = rest else {
      continue;
    };
    let name = rest
      .chars()
      .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
      .collect::<String>();
    if !name.is_empty() {
      defined.insert(name);
    }
  }

  // 2) Collect all callback function names referenced in `create_function`/`create_constructor`.
  //
  // These calls are often formatted over multiple lines, so scan across complete call blocks (from
  // the first line mentioning `create_function(` until the terminating `)?;` line).
  let mut referenced: BTreeSet<String> = BTreeSet::new();
  let mut in_create_call = false;
  let mut buf = String::new();

  for line in src.lines() {
    if !in_create_call {
      if line.contains("create_function(") || line.contains("create_constructor(") {
        in_create_call = true;
        buf.clear();
      } else {
        continue;
      }
    }

    buf.push_str(line);
    buf.push('\n');

    if !line.contains(")?;") {
      continue;
    }

    // Call complete; extract any `foo::<Host, R>` occurrences.
    in_create_call = false;
    let mut haystack = buf.as_str();
    while let Some(idx) = haystack.find("::<Host, R>") {
      let before = &haystack[..idx];
      let start = before
        .rfind(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .map(|i| i + 1)
        .unwrap_or(0);
      let name = &before[start..];
      if !name.is_empty() {
        referenced.insert(name.to_string());
      }
      haystack = &haystack[idx + "::<Host, R>".len()..];
    }
    buf.clear();
  }

  let missing: Vec<String> = referenced
    .into_iter()
    .filter(|name| !defined.contains(name))
    .collect();
  if missing.is_empty() {
    return Ok(());
  }

  bail!(
    "generated legacy WebIDL bindings reference undefined callbacks in module `{}`: {}",
    module_name,
    missing.join(", ")
  );
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn duplicate_fn_names_in_window_module_are_rejected() {
    let src = r#"
pub mod window {
  pub fn a() {}
  pub fn a() {}
}
"#;

    let err = ensure_no_duplicate_rust_fns_in_bindings_modules(src).unwrap_err();
    let msg = err.to_string();
    assert!(
      msg.contains("module `window`"),
      "expected module name in error message, got: {msg}"
    );
    assert!(
      msg.contains("a (2x)"),
      "expected duplicate fn name/count in error message, got: {msg}"
    );
  }

  #[test]
  fn same_fn_name_in_window_and_worker_is_allowed() {
    let src = r#"
pub mod window {
  pub fn a() {}
}

pub mod worker {
  pub fn a() {}
}
"#;

    ensure_no_duplicate_rust_fns_in_bindings_modules(src)
      .expect("expected no duplicates per module");
  }

  #[test]
  fn missing_bindings_modules_are_ignored() {
    let src = r#"
pub fn a() {}
pub fn a() {}
"#;

    ensure_no_duplicate_rust_fns_in_bindings_modules(src)
      .expect("expected duplicate checks to be skipped without window/worker modules");
  }
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
  world: &ResolvedWebIdlWorld,
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
        "allowlisted interface `{}` is missing from the WebIDL world",
        entry.name
      )
    })?;

    out.insert(
      entry.name.clone(),
      window_parse_interface_entry(iface, entry)?,
    );
  }

  Ok(out)
}

fn window_parse_interface_entry(
  iface: &ResolvedInterface,
  allow: &WindowBindingsAllowlistInterface,
) -> Result<WebIdlInterfaceAllowlist> {
  // Constructors.
  if allow.constructors {
    let mut found_ctor = false;
    for member in &iface.members {
      if member.name.as_deref() != Some("constructor") {
        continue;
      }
      let parsed = crate::webidl::parse_interface_member(&member.raw).with_context(|| {
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
        "Window bindings allowlist requested constructors for `{}`, but none were found",
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
    for member in &iface.members {
      if member.name.as_deref() != Some(attr_name.as_str()) {
        continue;
      }
      let parsed = crate::webidl::parse_interface_member(&member.raw).with_context(|| {
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
    for member in &iface.members {
      // Most operations can be matched by name directly, but WebIDL's `stringifier;` shorthand has
      // the synthetic member name `"stringifier"` even though it defines `toString()`.
      if member.name.as_deref() != Some(op_name.as_str())
        && !(op_name == "toString" && member.name.as_deref() == Some("stringifier"))
      {
        continue;
      }
      let parsed = crate::webidl::parse_interface_member(&member.raw).with_context(|| {
        format!(
          "failed to parse WebIDL member `{}` operation `{}`",
          iface.name, member.raw
        )
      })?;
      let InterfaceMember::Operation {
        name: Some(name), ..
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
    let mut saw_constructor_with_args = false;

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
      if arguments.is_empty() {
        constructible = true;
      } else {
        // MVP DOM bindings currently only support `constructor()` (no-argument) wiring. Some specs
        // (or our combined WebIDL snapshot) may include additional constructor overloads with
        // arguments; treat those as unsupported-but-ignorable as long as a no-arg constructor also
        // exists.
        saw_constructor_with_args = true;
      }
    }
    if !constructible {
      if saw_constructor_with_args {
        bail!(
          "DOM allowlist requested constructors for `{}`, but WORLD only provides constructors with arguments (MVP DOM bindings only support `constructor()`)",
          iface.name
        );
      }
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

  out.push_str(
    "// @generated by `bash scripts/cargo_agent.sh xtask webidl-bindings`. DO NOT EDIT.\n",
  );
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
  use webidl::ir::{IdlType, NamedType, NamedTypeKind, StringType};

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

  out.push_str(
    "// @generated by `bash scripts/cargo_agent.sh xtask webidl-bindings`. DO NOT EDIT.\n",
  );
  out.push_str("//\n");
  out.push_str("// Source inputs:\n");
  out.push_str(
    "// - src/webidl/generated/mod.rs (committed snapshot; produced by `bash scripts/cargo_agent.sh xtask webidl`)\n",
  );
  out.push_str("// - tools/webidl/local/chrome.idl (FastRender-local chrome bridge APIs)\n");
  out.push_str("\n");
  // Legacy bindings call `binding_value_to_js` to translate host-returned values into the
  // `webidl-js-runtime` value representation. The per-target modules (`window`/`worker`) import the
  // helper from their parent module, so it must be in scope here.
  out.push_str("use super::host::{binding_value_to_js, BindingValue, WebHostBindings};\n\n");

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

fn generate_bindings_module_unformatted_vmjs(
  resolved: &ResolvedWebIdlWorld,
  exposure_target: ExposureTarget,
  config: &WebIdlBindingsCodegenConfig,
) -> Result<String> {
  let mut out = String::new();

  out.push_str(
    "// @generated by `bash scripts/cargo_agent.sh xtask webidl-bindings`. DO NOT EDIT.\n",
  );
  out.push_str("//\n");
  out.push_str("// Source inputs:\n");
  out.push_str(
    "// - src/webidl/generated/mod.rs (committed snapshot; produced by `bash scripts/cargo_agent.sh xtask webidl`)\n",
  );
  out.push_str("// - tools/webidl/local/chrome.idl (FastRender-local chrome bridge APIs)\n");
  out.push_str("\n");

  let targets: &[ExposureTarget] = match exposure_target {
    ExposureTarget::All => &[ExposureTarget::Window, ExposureTarget::Worker],
    ExposureTarget::Window => &[ExposureTarget::Window],
    ExposureTarget::Worker => &[ExposureTarget::Worker],
  };

  let mut reexports: Vec<(String, String)> = Vec::new();
  let mut window_interface_reexports: Vec<String> = Vec::new();

  for target in targets {
    let (module_name, install_fn_name, globals): (&str, &str, &[&str]) = match target {
      ExposureTarget::All => unreachable!(),
      ExposureTarget::Window => ("window", "install_window_bindings_vm_js", &["Window"]),
      ExposureTarget::Worker => (
        "worker",
        "install_worker_bindings_vm_js",
        &["WorkerGlobalScope"],
      ),
    };

    let filtered = resolved.filter_by_exposure(*target);
    let analyzed = crate::webidl::analyze::analyze_resolved_world(&filtered);
    let (inner, interface_reexports) = generate_bindings_module_for_target_vmjs_unformatted(
      &filtered,
      &analyzed,
      config,
      install_fn_name,
      globals,
    )?;

    out.push_str(&format!("pub mod {module_name} {{\n"));
    out.push_str(&indent_lines(&inner, 2));
    out.push_str("}\n\n");

    reexports.push((module_name.to_string(), install_fn_name.to_string()));
    if *target == ExposureTarget::Window {
      window_interface_reexports = interface_reexports;
    }
  }

  for install_fn_name in window_interface_reexports {
    out.push_str(&format!("pub use window::{install_fn_name};\n"));
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
  let referenced_ctx = collect_referenced_type_context_entries(type_ctx, &selected);

  let mut out = String::new();

  out.push_str("use std::collections::BTreeMap;\n");
  out.push_str("use std::sync::OnceLock;\n\n");
  out.push_str("use super::{binding_value_to_js, BindingValue, WebHostBindings};\n");
  out.push_str("use crate::js::webidl::DataPropertyAttributes;\n");
  out.push_str(
    "#[allow(unused_imports)]\nuse webidl_js_runtime::{convert_arguments, resolve_overload, ArgumentSchema, ConvertedValue, Optionality, OverloadArg, OverloadSig, WebIdlJsRuntime};\n",
  );
  out.push_str(
    "#[allow(unused_imports)]\nuse webidl::ir::{DefaultValue, DictionaryMemberSchema, DictionarySchema, IdlType, NamedType, NamedTypeKind, NumericLiteral, NumericType, StringType, TypeAnnotation, TypeContext};\n\n",
  );

  // The bindings runtime trait (`crate::js::webidl::WebIdlBindingsRuntime`) and the core WebIDL
  // runtime trait (`webidl_js_runtime::WebIdlJsRuntime`) both expose associated types named
  // `JsValue`/`PropertyKey`/`Error`. The generated bindings frequently need both traits, so define
  // disambiguating aliases based on the bindings runtime trait.
  out.push_str(
    "type RtJsValue<Host, R> = <R as crate::js::webidl::WebIdlBindingsRuntime<Host>>::JsValue;\n",
  );
  out.push_str(
    "type RtPropertyKey<Host, R> = <R as crate::js::webidl::WebIdlBindingsRuntime<Host>>::PropertyKey;\n",
  );
  out.push_str(
    "type RtError<Host, R> = <R as crate::js::webidl::WebIdlBindingsRuntime<Host>>::Error;\n\n",
  );

  // Helper functions used to disambiguate method calls when both the bindings runtime trait
  // (`crate::js::webidl::WebIdlBindingsRuntime`) and the core WebIDL runtime trait
  // (`webidl_js_runtime::WebIdlJsRuntime`) are in scope. Both traits expose methods like
  // `throw_type_error`, `js_number`, etc.
  out.push_str("#[inline]\n#[allow(dead_code)]\nfn rt_throw_type_error<Host, R>(\n  rt: &mut R,\n  message: &str,\n) -> RtError<Host, R>\nwhere\n  R: crate::js::webidl::WebIdlBindingsRuntime<Host>,\n{\n  rt.throw_type_error(message)\n}\n\n");
  out.push_str("#[inline]\n#[allow(dead_code)]\nfn rt_is_object<Host, R>(rt: &R, value: RtJsValue<Host, R>) -> bool\nwhere\n  R: crate::js::webidl::WebIdlBindingsRuntime<Host>,\n{\n  rt.is_object(value)\n}\n\n");
  out.push_str("#[inline]\n#[allow(dead_code)]\nfn rt_js_undefined<Host, R>(rt: &R) -> RtJsValue<Host, R>\nwhere\n  R: crate::js::webidl::WebIdlBindingsRuntime<Host>,\n{\n  rt.js_undefined()\n}\n\n");
  out.push_str("#[inline]\n#[allow(dead_code)]\nfn rt_js_null<Host, R>(rt: &R) -> RtJsValue<Host, R>\nwhere\n  R: crate::js::webidl::WebIdlBindingsRuntime<Host>,\n{\n  rt.js_null()\n}\n\n");
  out.push_str("#[inline]\n#[allow(dead_code)]\nfn rt_js_number<Host, R>(rt: &R, value: f64) -> RtJsValue<Host, R>\nwhere\n  R: crate::js::webidl::WebIdlBindingsRuntime<Host>,\n{\n  rt.js_number(value)\n}\n\n");
  out.push_str("#[inline]\n#[allow(dead_code)]\nfn rt_symbol_iterator<Host, R>(\n  rt: &mut R,\n) -> Result<RtPropertyKey<Host, R>, RtError<Host, R>>\nwhere\n  R: crate::js::webidl::WebIdlBindingsRuntime<Host>,\n{\n  rt.symbol_iterator()\n}\n\n");
  out.push_str("#[inline]\n#[allow(dead_code)]\nfn rt_symbol_async_iterator<Host, R>(\n  rt: &mut R,\n) -> Result<RtPropertyKey<Host, R>, RtError<Host, R>>\nwhere\n  R: crate::js::webidl::WebIdlBindingsRuntime<Host>,\n{\n  rt.symbol_async_iterator()\n}\n\n");

  // Shared WebIDL type context (enums, dictionaries, typedefs) used by conversions.
  out.push_str("fn type_context() -> &'static TypeContext {\n");
  out.push_str("  static CTX: OnceLock<TypeContext> = OnceLock::new();\n");
  out.push_str("  CTX.get_or_init(|| {\n");
  out.push_str("    let mut ctx = TypeContext::default();\n");
  out.push_str("\n");

  // Enums.
  for name in &referenced_ctx.enums {
    let values = type_ctx
      .enums
      .get(name)
      .with_context(|| format!("missing enum `{name}` in TypeContext"))?;
    out.push_str(&format!(
      "    ctx.add_enum({name_lit}, [{}]);\n",
      values
        .iter()
        .map(|v| rust_string_literal(v))
        .collect::<Vec<_>>()
        .join(", "),
      name_lit = rust_string_literal(name)
    ));
  }

  // Typedefs.
  for name in &referenced_ctx.typedefs {
    let ty = type_ctx
      .typedefs
      .get(name)
      .with_context(|| format!("missing typedef `{name}` in TypeContext"))?;
    out.push_str(&format!(
      "    ctx.add_typedef({name_lit}, {ty_expr});\n",
      name_lit = rust_string_literal(name),
      ty_expr = render_webidl_ir_type(ty),
    ));
  }

  // Dictionaries.
  for name in &referenced_ctx.dictionaries {
    let dict = type_ctx
      .dictionaries
      .get(name)
      .with_context(|| format!("missing dictionary `{name}` in TypeContext"))?;
    out.push_str(&format!(
      "    ctx.add_dictionary(DictionarySchema {{ name: {name_lit}.to_string(), inherits: {inherits}, members: vec![\n",
      name_lit = rust_string_literal(&dict.name),
      inherits = dict
        .inherits
        .as_ref()
        .map(|p| format!("Some({}.to_string())", rust_string_literal(p)))
        .unwrap_or_else(|| "None".to_string()),
    ));
    for member in &dict.members {
      out.push_str(&format!(
        "      DictionaryMemberSchema {{ name: {member_name}.to_string(), required: {required}, ty: {ty}, default: {default} }},\n",
        member_name = rust_string_literal(&member.name),
        required = if member.required { "true" } else { "false" },
        ty = render_webidl_ir_type(&member.ty),
        default = member
          .default
          .as_ref()
          .map(|d| format!("Some({})", render_webidl_ir_default_value(d)))
          .unwrap_or_else(|| "None".to_string()),
      ));
    }
    out.push_str("    ]});\n");
  }

  out.push_str("    ctx\n");
  out.push_str("  })\n");
  out.push_str("}\n\n");

  // Convert a `ConvertedValue` (from core WebIDL conversions) into the host-facing `BindingValue`.
  out.push_str("fn converted_value_to_binding_value<Host, R>(\n");
  out.push_str("  rt: &mut R,\n");
  out.push_str("  ctx: &TypeContext,\n");
  out.push_str("  ty: &IdlType,\n");
  out.push_str("  value: ConvertedValue<RtJsValue<Host, R>>,\n");
  out.push_str(") -> Result<BindingValue<RtJsValue<Host, R>>, RtError<Host, R>>\n");
  out.push_str("where\n");
  out.push_str("  R: crate::js::webidl::WebIdlBindingsRuntime<Host> + WebIdlJsRuntime<JsValue = RtJsValue<Host, R>, PropertyKey = RtPropertyKey<Host, R>, Error = RtError<Host, R>>,\n");
  out.push_str("{\n");
  out.push_str("  // Callback types are converted to raw JS values by `convert_arguments`, but the host expects\n");
  out.push_str("  // rooted callback handles so it can store and invoke them later.\n");
  out.push_str("  match ty {\n");
  out.push_str("    IdlType::Annotated { inner, .. } => {\n");
  out
    .push_str("      return converted_value_to_binding_value::<Host, R>(rt, ctx, inner, value);\n");
  out.push_str("    }\n");
  out.push_str("    IdlType::Nullable(inner) => {\n");
  out.push_str("      if matches!(value, ConvertedValue::Null) {\n");
  out.push_str("        return Ok(BindingValue::Null);\n");
  out.push_str("      }\n");
  out
    .push_str("      return converted_value_to_binding_value::<Host, R>(rt, ctx, inner, value);\n");
  out.push_str("    }\n");
  out
    .push_str("    IdlType::Named(NamedType { kind: NamedTypeKind::CallbackFunction, .. }) => {\n");
  out.push_str("      return match value {\n");
  out.push_str("        ConvertedValue::Null => Ok(BindingValue::Null),\n");
  out.push_str("        ConvertedValue::Any(v) | ConvertedValue::Object(v) => {\n");
  out.push_str("          Ok(BindingValue::Callback(rt.root_callback_function(v)?))\n");
  out.push_str("        }\n");
  out.push_str(
    "        _ => Err(rt_throw_type_error::<Host, R>(rt, \"expected callback function value\")),\n",
  );
  out.push_str("      };\n");
  out.push_str("    }\n");
  out.push_str(
    "    IdlType::Named(NamedType { kind: NamedTypeKind::CallbackInterface, .. }) => {\n",
  );
  out.push_str("      return match value {\n");
  out.push_str("        ConvertedValue::Null => Ok(BindingValue::Null),\n");
  out.push_str("        ConvertedValue::Any(v) | ConvertedValue::Object(v) => {\n");
  out.push_str("          Ok(BindingValue::Callback(rt.root_callback_interface(v)?))\n");
  out.push_str("        }\n");
  out.push_str(
    "        _ => Err(rt_throw_type_error::<Host, R>(rt, \"expected callback interface value\")),\n",
  );
  out.push_str("      };\n");
  out.push_str("    }\n");
  out.push_str("    _ => {}\n");
  out.push_str("  }\n");
  out.push_str("\n");
  out.push_str("  Ok(match value {\n");
  out.push_str("    ConvertedValue::Undefined => BindingValue::Undefined,\n");
  out.push_str("    ConvertedValue::Null => BindingValue::Null,\n");
  out.push_str("    ConvertedValue::Boolean(b) => BindingValue::Bool(b),\n");
  out.push_str("    ConvertedValue::Byte(n) => BindingValue::Number(n as f64),\n");
  out.push_str("    ConvertedValue::Octet(n) => BindingValue::Number(n as f64),\n");
  out.push_str("    ConvertedValue::Short(n) => BindingValue::Number(n as f64),\n");
  out.push_str("    ConvertedValue::UnsignedShort(n) => BindingValue::Number(n as f64),\n");
  out.push_str("    ConvertedValue::Long(n) => BindingValue::Number(n as f64),\n");
  out.push_str("    ConvertedValue::UnsignedLong(n) => BindingValue::Number(n as f64),\n");
  out.push_str("    ConvertedValue::LongLong(n) => BindingValue::Number(n as f64),\n");
  out.push_str("    ConvertedValue::UnsignedLongLong(n) => BindingValue::Number(n as f64),\n");
  out.push_str("    ConvertedValue::Float(n) => BindingValue::Number(n as f64),\n");
  out.push_str("    ConvertedValue::UnrestrictedFloat(n) => BindingValue::Number(n as f64),\n");
  out.push_str("    ConvertedValue::Double(n) => BindingValue::Number(n),\n");
  out.push_str("    ConvertedValue::UnrestrictedDouble(n) => BindingValue::Number(n),\n");
  out.push_str(
    "    ConvertedValue::String(s) | ConvertedValue::Enum(s) => BindingValue::String(s),\n",
  );
  out.push_str(
    "    ConvertedValue::Any(v) | ConvertedValue::Object(v) => BindingValue::Object(v),\n",
  );
  out.push_str("    ConvertedValue::PlatformObject(obj) => {\n");
  out.push_str("      let Some(v) = rt.platform_object_to_js_value(&obj) else {\n");
  out.push_str(
    "        return Err(rt_throw_type_error::<Host, R>(rt, \"Unsupported platform object value for this runtime\"));\n",
  );
  out.push_str("      };\n");
  out.push_str("      BindingValue::Object(v)\n");
  out.push_str("    }\n");
  // `Promise<T>` and `async sequence<T>` conversions currently surface as opaque JS objects to the
  // host (the element/inner types are metadata; iteration/awaiting happens in host code).
  out.push_str("    ConvertedValue::Promise { promise, .. } => BindingValue::Object(promise),\n");
  out.push_str(
    "    ConvertedValue::AsyncSequence { object, .. } => BindingValue::Object(object),\n",
  );
  out.push_str("    ConvertedValue::Sequence { elem_ty, values } => {\n");
  out.push_str(
    "      let mut out_values: Vec<BindingValue<RtJsValue<Host, R>>> = Vec::with_capacity(values.len());\n",
  );
  out.push_str("      for item in values {\n");
  out.push_str(
    "        out_values.push(converted_value_to_binding_value::<Host, R>(rt, ctx, &elem_ty, item)?);\n",
  );
  out.push_str("      }\n");
  out.push_str("      if matches!(ty, IdlType::FrozenArray(_)) {\n");
  out.push_str("        BindingValue::FrozenArray(out_values)\n");
  out.push_str("      } else {\n");
  out.push_str("        BindingValue::Sequence(out_values)\n");
  out.push_str("      }\n");
  out.push_str("    }\n");
  out.push_str("    ConvertedValue::Record { value_ty, entries, .. } => {\n");
  out.push_str("      let mut out: Vec<(String, BindingValue<RtJsValue<Host, R>>)> = Vec::with_capacity(entries.len());\n");
  out.push_str("      for (k, v) in entries {\n");
  out.push_str(
    "        out.push((k, converted_value_to_binding_value::<Host, R>(rt, ctx, &value_ty, v)?));\n",
  );
  out.push_str("      }\n");
  out.push_str("      BindingValue::Record(out)\n");
  out.push_str("    }\n");
  out.push_str("    ConvertedValue::Dictionary { name, members } => {\n");
  out.push_str(
    "      let mut map: BTreeMap<String, BindingValue<RtJsValue<Host, R>>> = BTreeMap::new();\n",
  );
  out.push_str("      let fallback = IdlType::Any;\n");
  out.push_str(
    "      let member_schemas = ctx.flattened_dictionary_members(&name).unwrap_or_default();\n",
  );
  out.push_str("      for (k, v) in members {\n");
  out.push_str("        let member_ty = member_schemas\n");
  out.push_str("          .iter()\n");
  out.push_str("          .find(|m| m.name == k)\n");
  out.push_str("          .map(|m| &m.ty)\n");
  out.push_str("          .unwrap_or(&fallback);\n");
  out.push_str(
    "        map.insert(k, converted_value_to_binding_value::<Host, R>(rt, ctx, member_ty, v)?);\n",
  );
  out.push_str("      }\n");
  out.push_str("      BindingValue::Dictionary(map)\n");
  out.push_str("    }\n");
  out.push_str("    ConvertedValue::Union { member_ty, value } => {\n");
  out.push_str("      let member_type = member_ty.to_string();\n");
  out.push_str(
    "      let value = converted_value_to_binding_value::<Host, R>(rt, ctx, &member_ty, *value)?;\n",
  );
  out.push_str("      BindingValue::Union {\n");
  out.push_str("        member_type,\n");
  out.push_str("        value: Box::new(value),\n");
  out.push_str("      }\n");
  out.push_str("    }\n");
  out.push_str("  })\n");
  out.push_str("}\n\n");

  // Operation shims.
  #[derive(Debug, Clone, Copy, PartialEq, Eq)]
  enum AttrWrapperKind {
    Getter,
    Setter,
  }
  let mut emitted_attr_wrappers: BTreeMap<String, (String, String, bool, AttrWrapperKind)> =
    BTreeMap::new();
  let mut ensure_unique_attr_wrapper = |wrapper_name: String,
                                        origin: (String, String, bool, AttrWrapperKind)|
   -> Result<bool> {
    use std::collections::btree_map::Entry;
    match emitted_attr_wrappers.entry(wrapper_name) {
      Entry::Vacant(v) => {
        v.insert(origin);
        Ok(true)
      }
      Entry::Occupied(o) => {
        if *o.get() == origin {
          Ok(false)
        } else {
          bail!(
              "WebIDL bindings codegen attempted to emit duplicate attribute wrapper `{}` for {:?} and {:?}",
              o.key(),
              o.get(),
              origin
            );
        }
      }
    }
  };
  for iface in selected.values() {
    let global = is_global_iface(&iface.name);
    for (op_name, overloads) in &iface.operations {
      write_operation_wrapper(
        &mut out,
        resolved,
        &type_ctx,
        &iface.name,
        op_name,
        iface.iterable.as_ref(),
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
        iface.iterable.as_ref(),
        overloads,
        true,
        global,
        config,
      )?;
    }
    for attr in iface.attributes.values() {
      let origin = (
        iface.name.clone(),
        attr.name.clone(),
        false,
        AttrWrapperKind::Getter,
      );
      if ensure_unique_attr_wrapper(attr_getter_fn_name(&iface.name, &attr.name, false), origin)? {
        write_attribute_getter_wrapper(&mut out, &iface.name, &attr.name, false);
      }
      if !attr.readonly {
        let origin = (
          iface.name.clone(),
          attr.name.clone(),
          false,
          AttrWrapperKind::Setter,
        );
        if ensure_unique_attr_wrapper(attr_setter_fn_name(&iface.name, &attr.name, false), origin)?
        {
          write_attribute_setter_wrapper(
            &mut out,
            resolved,
            &iface.name,
            &attr.name,
            &attr.type_,
            false,
          )?;
        }
      }
    }
    for attr in iface.static_attributes.values() {
      let origin = (
        iface.name.clone(),
        attr.name.clone(),
        true,
        AttrWrapperKind::Getter,
      );
      if ensure_unique_attr_wrapper(attr_getter_fn_name(&iface.name, &attr.name, true), origin)? {
        write_attribute_getter_wrapper(&mut out, &iface.name, &attr.name, true);
      }
      if !attr.readonly {
        let origin = (
          iface.name.clone(),
          attr.name.clone(),
          true,
          AttrWrapperKind::Setter,
        );
        if ensure_unique_attr_wrapper(attr_setter_fn_name(&iface.name, &attr.name, true), origin)? {
          write_attribute_setter_wrapper(
            &mut out,
            resolved,
            &iface.name,
            &attr.name,
            &attr.type_,
            true,
          )?;
        }
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
    "pub fn {install_fn_name}<Host, R>(rt: &mut R, host: &mut Host) -> Result<(), RtError<Host, R>>\n"
  ));
  out.push_str("where\n");
  out.push_str("  R: crate::js::webidl::WebIdlBindingsRuntime<Host> + WebIdlJsRuntime<JsValue = RtJsValue<Host, R>, PropertyKey = RtPropertyKey<Host, R>, Error = RtError<Host, R>>,\n");
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
        if iface.iterable.as_ref().is_some_and(|it| it.async_) {
          out.push_str(&format!(
            "  let iterator_key = rt_symbol_async_iterator::<Host, R>(rt)?;\n  <R as crate::js::webidl::WebIdlBindingsRuntime<Host>>::define_data_property(rt, {proto}, iterator_key, func, DataPropertyAttributes::METHOD)?;\n",
            proto = proto_var.as_str()
          ));
        } else {
          out.push_str(&format!(
            "  let iterator_key = rt_symbol_iterator::<Host, R>(rt)?;\n  <R as crate::js::webidl::WebIdlBindingsRuntime<Host>>::define_data_property(rt, {proto}, iterator_key, func, DataPropertyAttributes::METHOD)?;\n",
            proto = proto_var.as_str()
          ));
        }
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
        out.push_str("  let set = rt_js_undefined::<Host, R>(rt);\n");
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
        out.push_str("  let set = rt_js_undefined::<Host, R>(rt);\n");
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
      out.push_str(&format!(
        "  rt.define_constant({proto}, \"{name}\", {expr})?;\n",
        proto = proto_var.as_str(),
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

fn generate_bindings_module_for_target_vmjs_unformatted(
  resolved: &ResolvedWebIdlWorld,
  analyzed: &AnalyzedWebIdlWorld,
  config: &WebIdlBindingsCodegenConfig,
  install_fn_name: &str,
  global_interfaces: &[&str],
) -> Result<(String, Vec<String>)> {
  let is_global_iface = |name: &str| global_interfaces.iter().any(|g| *g == name);

  let selected = select_interfaces(resolved, analyzed, config)?;
  // Prototype chains are installed by looking up the parent constructor's `.prototype` from the
  // global object. That means parent prototypes must be installed before derived interfaces.
  //
  // Keep the *generated* per-interface installer functions in stable name order (matching
  // `selected`, a `BTreeMap`), but emit the *aggregator* (`install_window_bindings_vm_js`) in
  // parent-before-child order so prototype chains can be wired correctly on first install.
  let install_order = order_selected_interfaces_by_inheritance_vmjs(&selected, &is_global_iface)?;
  let type_ctx = build_type_context(resolved).context("build WebIDL type context")?;
  let referenced_dicts = collect_referenced_dictionaries(resolved, &type_ctx, &selected);

  let mut out = String::new();

  let needs_accessor_property_attributes = selected
    .values()
    .any(|iface| !iface.attributes.is_empty() || !iface.static_attributes.is_empty());
  let needs_iterable_kind = selected.values().any(|iface| iface.iterable.is_some());

  out.push_str(
    "use vm_js::{GcObject, Heap, Realm, Scope, Value, Vm, VmError, VmHost, VmHostHooks};\n",
  );
  out.push_str(
    "use webidl_vm_js::bindings_runtime::{AccessorPropertyAttributes, BindingValue, BindingsRuntime, DataPropertyAttributes, to_int32_f64, to_uint32_f64};\n",
  );
  out.push_str("use webidl_vm_js::conversions;\n");
  if needs_iterable_kind {
    out.push_str("use webidl_vm_js::{host_from_hooks, IterableKind};\n\n");
  } else {
    out.push_str("use webidl_vm_js::host_from_hooks;\n\n");
  }

  // Dictionary conversion helpers (sorted).
  for dict_name in &referenced_dicts {
    if let Some(dict) = resolved.dictionaries.get(dict_name) {
      write_dictionary_converter_vmjs(&mut out, resolved, dict);
    }
  }

  // Operation shims.
  for iface in selected.values() {
    let global = is_global_iface(&iface.name);
    for (op_name, overloads) in &iface.operations {
      write_operation_wrapper_vmjs(
        &mut out,
        resolved,
        &iface.name,
        op_name,
        iface.iterable.as_ref(),
        overloads,
        false,
        global,
      );
    }
    for (op_name, overloads) in &iface.static_operations {
      write_operation_wrapper_vmjs(
        &mut out,
        resolved,
        &iface.name,
        op_name,
        None,
        overloads,
        true,
        global,
      );
    }
    for attr in iface.attributes.values() {
      write_attribute_getter_wrapper_vmjs(&mut out, &iface.name, attr, false, global);
      if !attr.readonly {
        write_attribute_setter_wrapper_vmjs(&mut out, resolved, &iface.name, attr, false, global);
      }
    }
    for attr in iface.static_attributes.values() {
      write_attribute_getter_wrapper_vmjs(&mut out, &iface.name, attr, true, global);
      if !attr.readonly {
        write_attribute_setter_wrapper_vmjs(&mut out, resolved, &iface.name, attr, true, global);
      }
    }
    if !global {
      write_constructor_wrapper_vmjs(&mut out, resolved, &iface.name, &iface.constructors);
    }
  }

  let mut interface_installers: Vec<String> = Vec::new();
  let mut interface_installer_by_name: BTreeMap<String, String> = BTreeMap::new();

  for iface in selected.values() {
    let global_iface = is_global_iface(&iface.name);
    let install_name = if global_iface {
      format!(
        "install_{}_ops_bindings_vm_js",
        to_snake_public_ident(&iface.name)
      )
    } else {
      format!(
        "install_{}_bindings_vm_js",
        to_snake_public_ident(&iface.name)
      )
    };
    interface_installers.push(install_name.clone());
    interface_installer_by_name.insert(iface.name.clone(), install_name.clone());

    out.push_str(&format!(
      "pub fn {install_name}(\n  vm: &mut Vm,\n  heap: &mut Heap,\n  realm: &Realm,\n) -> Result<(), VmError> {{\n",
    ));
    out.push_str("  let mut rt = BindingsRuntime::new(vm, heap);\n");
    out.push_str("  let global = realm.global_object();\n");
    out.push_str("  rt.scope.push_root(Value::Object(global))?;\n\n");

    // Install in "merge" mode so generated bindings can coexist with handwritten globals.

    out.push_str("  let global_var_attrs = DataPropertyAttributes::new(true, false, true);\n");

    if global_iface {
      // Global functions live on the global object.
      for (op_name, overloads) in &iface.operations {
        let length = overloads
          .iter()
          .map(|sig| required_arg_count(&sig.arguments))
          .min()
          .unwrap_or(0) as u32;
        out.push_str(&format!(
          "  {{\n    let key = rt.property_key({name_lit})?;\n    if rt.scope.heap().object_get_own_property(global, &key)?.is_none() {{\n      let func = rt.alloc_native_function({func}, None, {name_lit}, {length})?;\n      rt.define_data_property_str(global, {name_lit}, Value::Object(func), global_var_attrs)?;\n    }}\n  }}\n",
          func = op_wrapper_fn_name(&iface.name, op_name),
          name_lit = rust_string_literal(op_name),
          length = length,
        ));
      }
      for attr in iface.attributes.values() {
        out.push_str(&format!(
          "  {{\n    let key = rt.property_key({attr_lit})?;\n    if rt.scope.heap().object_get_own_property(global, &key)?.is_none() {{\n      let get = rt.alloc_native_function({getter}, None, {get_name_lit}, 0)?;\n",
          attr_lit = rust_string_literal(&attr.name),
          getter = attr_getter_fn_name(&iface.name, &attr.name, false),
          get_name_lit = rust_string_literal(&format!("get {}", attr.name)),
        ));
        if attr.readonly {
          out.push_str("      let set = Value::Undefined;\n");
        } else {
          out.push_str(&format!(
            "      let set = Value::Object(rt.alloc_native_function({setter}, None, {set_name_lit}, 1)?);\n",
            setter = attr_setter_fn_name(&iface.name, &attr.name, false),
            set_name_lit = rust_string_literal(&format!("set {}", attr.name)),
          ));
        }
        out.push_str(&format!(
          "      rt.define_accessor_property_str(global, {attr_lit}, Value::Object(get), set, AccessorPropertyAttributes::ATTRIBUTE)?;\n    }}\n  }}\n",
          attr_lit = rust_string_literal(&attr.name),
        ));
      }
      for attr in iface.static_attributes.values() {
        out.push_str(&format!(
          "  {{\n    let key = rt.property_key({attr_lit})?;\n    if rt.scope.heap().object_get_own_property(global, &key)?.is_none() {{\n      let get = rt.alloc_native_function({getter}, None, {get_name_lit}, 0)?;\n",
          attr_lit = rust_string_literal(&attr.name),
          getter = attr_getter_fn_name(&iface.name, &attr.name, true),
          get_name_lit = rust_string_literal(&format!("get {}", attr.name)),
        ));
        if attr.readonly {
          out.push_str("      let set = Value::Undefined;\n");
        } else {
          out.push_str(&format!(
            "      let set = Value::Object(rt.alloc_native_function({setter}, None, {set_name_lit}, 1)?);\n",
            setter = attr_setter_fn_name(&iface.name, &attr.name, true),
            set_name_lit = rust_string_literal(&format!("set {}", attr.name)),
          ));
        }
        out.push_str(&format!(
          "      rt.define_accessor_property_str(global, {attr_lit}, Value::Object(get), set, AccessorPropertyAttributes::ATTRIBUTE)?;\n    }}\n  }}\n",
          attr_lit = rust_string_literal(&attr.name),
        ));
      }
      out.push_str("  Ok(())\n");
      out.push_str("}\n\n");
      continue;
    }

    out.push_str("  let ctor_link_attrs = DataPropertyAttributes::new(false, false, false);\n\n");

    let ctor_var = format!("ctor_{}", to_snake_ident(&iface.name));
    let proto_var = format!("proto_{}", to_snake_ident(&iface.name));
    let iterable_iterator_alias = iface.iterable.as_ref().map(|it| {
      if it.key_type.is_some() {
        "entries"
      } else {
        "values"
      }
    });

    // Constructor function (even for static-only interfaces like URL).
    let ctor_call_fn = ctor_call_without_new_fn_name(&iface.name);
    let ctor_construct_fn = ctor_construct_fn_name(&iface.name);
    let construct_expr = format!("Some({ctor_construct_fn})");
    let ctor_length = iface
      .constructors
      .iter()
      .map(|sig| required_arg_count(&sig.arguments) as u32)
      .min()
      .unwrap_or(0);

    let uses_ctor = !iface.static_operations.is_empty()
      || !iface.static_attributes.is_empty()
      || !iface.constants.is_empty();
    let uses_proto = !iface.operations.is_empty()
      || !iface.attributes.is_empty()
      || !iface.constants.is_empty()
      || (config.prototype_chains
        && iface
          .inherits
          .as_deref()
          .is_some_and(|parent| !is_global_iface(parent)));
    let ctor_binding = if uses_ctor {
      ctor_var.clone()
    } else {
      format!("_{ctor_var}")
    };
    let proto_binding = if uses_proto {
      proto_var.clone()
    } else {
      format!("_{proto_var}")
    };

    // Acquire constructor + prototype objects, reusing existing values when present.
    out.push_str(&format!(
      "  let ({ctor_var}, {proto_var}) = {{\n    let ctor_key = rt.property_key({name_lit})?;\n    let ctor_value = rt\n      .scope\n      .heap()\n      .object_get_own_data_property_value(global, &ctor_key)?\n      .unwrap_or(Value::Undefined);\n    if let Value::Object(ctor_obj) = ctor_value {{\n      let proto_key = rt.property_key(\"prototype\")?;\n      let proto_value = rt.vm.get(&mut rt.scope, ctor_obj, proto_key)?;\n      let proto_obj = if let Value::Object(proto_obj) = proto_value {{\n        proto_obj\n      }} else {{\n        let proto_obj = rt.alloc_object()?;\n        rt.define_data_property_str(ctor_obj, \"prototype\", Value::Object(proto_obj), ctor_link_attrs)?;\n        proto_obj\n      }};\n      let constructor_key = rt.property_key(\"constructor\")?;\n      if rt\n        .scope\n        .heap()\n        .object_get_own_property(proto_obj, &constructor_key)?\n        .is_none()\n      {{\n        rt.define_data_property_str(\n          proto_obj,\n          \"constructor\",\n          Value::Object(ctor_obj),\n          ctor_link_attrs,\n        )?;\n      }}\n      (ctor_obj, proto_obj)\n    }} else {{\n      let proto_obj = rt.alloc_object()?;\n      let slots = [Value::Object(proto_obj)];\n      let ctor_obj = rt.alloc_native_function_with_slots(\n        {ctor_call_fn},\n        {construct_expr},\n        {name_lit},\n        {ctor_length},\n        &slots,\n      )?;\n      rt.define_data_property_str(global, {name_lit}, Value::Object(ctor_obj), global_var_attrs)?;\n      rt.define_data_property_str(ctor_obj, \"prototype\", Value::Object(proto_obj), ctor_link_attrs)?;\n      rt.define_data_property_str(proto_obj, \"constructor\", Value::Object(ctor_obj), ctor_link_attrs)?;\n      (ctor_obj, proto_obj)\n    }}\n  }};\n\n",
      ctor_var = ctor_binding,
      proto_var = proto_binding,
      name_lit = rust_string_literal(&iface.name),
      ctor_call_fn = ctor_call_fn,
      construct_expr = construct_expr,
      ctor_length = ctor_length,
    ));

    if config.prototype_chains {
      if let Some(parent) = iface.inherits.as_deref() {
        if !is_global_iface(parent) {
          // Look up the parent prototype object from the existing global bindings (installed earlier
          // by the aggregator or supplied by the embedder).
          out.push_str(&format!(
            "  let parent_proto = {{\n    let ctor_key = rt.property_key({parent_lit})?;\n    let ctor_value = rt\n      .scope\n      .heap()\n      .object_get_own_data_property_value(global, &ctor_key)?\n      .unwrap_or(Value::Undefined);\n    if let Value::Object(ctor_obj) = ctor_value {{\n      let proto_key = rt.property_key(\"prototype\")?;\n      match rt.vm.get(&mut rt.scope, ctor_obj, proto_key)? {{\n        Value::Object(obj) => Some(obj),\n        _ => None,\n      }}\n    }} else {{\n      None\n    }}\n  }};\n  if let Some(parent_proto) = parent_proto {{\n    rt.set_prototype({child_proto}, Some(parent_proto))?;\n  }}\n\n",
            parent_lit = rust_string_literal(parent),
            child_proto = proto_var,
          ));
        }
      }
    }

    // Prototype methods.
    for (op_name, overloads) in &iface.operations {
      let length = overloads
        .iter()
        .map(|sig| required_arg_count(&sig.arguments))
        .min()
        .unwrap_or(0) as u32;
      let is_iterable_iterator_alias =
        iterable_iterator_alias.is_some_and(|target| target == op_name.as_str());
      if is_iterable_iterator_alias {
        let iterator_key = if iface.iterable.as_ref().is_some_and(|it| it.async_) {
          "realm.well_known_symbols().async_iterator"
        } else {
          "realm.well_known_symbols().iterator"
        };
        out.push_str(&format!(
          "  {{\n    let key = rt.property_key({name_lit})?;\n    let installed = rt.scope.heap().object_get_own_property({proto_var}, &key)?.is_some();\n    let func = if installed {{\n      None\n    }} else {{\n      let func = rt.alloc_native_function({func}, None, {name_lit}, {length})?;\n      rt.define_data_property_str(\n        {proto_var},\n        {name_lit},\n        Value::Object(func),\n        DataPropertyAttributes::METHOD,\n      )?;\n      Some(func)\n    }};\n\n    let iterator_key = vm_js::PropertyKey::from_symbol({iterator_key});\n    if rt\n      .scope\n      .heap()\n      .object_get_own_property({proto_var}, &iterator_key)?\n      .is_none()\n    {{\n      if let Some(func) = func {{\n        rt.define_data_property(\n          {proto_var},\n          iterator_key,\n          Value::Object(func),\n          DataPropertyAttributes::METHOD,\n        )?;\n      }} else {{\n        match rt.vm.get(&mut rt.scope, {proto_var}, key)? {{\n          Value::Object(existing) => {{\n            rt.define_data_property(\n              {proto_var},\n              iterator_key,\n              Value::Object(existing),\n              DataPropertyAttributes::METHOD,\n            )?;\n          }}\n          _ => {{}}\n        }}\n      }}\n    }}\n  }}\n",
          func = op_wrapper_fn_name(&iface.name, op_name),
          name_lit = rust_string_literal(op_name),
          proto_var = proto_var,
          length = length,
          iterator_key = iterator_key,
        ));
      } else {
        out.push_str(&format!(
          "  {{\n    let key = rt.property_key({name_lit})?;\n    if rt.scope.heap().object_get_own_property({proto_var}, &key)?.is_none() {{\n      let func = rt.alloc_native_function({func}, None, {name_lit}, {length})?;\n      rt.define_data_property_str(\n        {proto_var},\n        {name_lit},\n        Value::Object(func),\n        DataPropertyAttributes::METHOD,\n      )?;\n    }}\n  }}\n",
          func = op_wrapper_fn_name(&iface.name, op_name),
          name_lit = rust_string_literal(op_name),
          proto_var = proto_var,
          length = length,
        ));
      }
    }

    // Prototype attributes.
    for attr in iface.attributes.values() {
      out.push_str(&format!(
        "  {{\n    let key = rt.property_key({attr_lit})?;\n    if rt.scope.heap().object_get_own_property({proto_var}, &key)?.is_none() {{\n      let get = rt.alloc_native_function({getter}, None, {get_name_lit}, 0)?;\n",
        proto_var = proto_var,
        attr_lit = rust_string_literal(&attr.name),
        getter = attr_getter_fn_name(&iface.name, &attr.name, false),
        get_name_lit = rust_string_literal(&format!("get {}", attr.name)),
      ));
      if attr.readonly {
        out.push_str("      let set = Value::Undefined;\n");
      } else {
        out.push_str(&format!(
          "      let set = Value::Object(rt.alloc_native_function({setter}, None, {set_name_lit}, 1)?);\n",
          setter = attr_setter_fn_name(&iface.name, &attr.name, false),
          set_name_lit = rust_string_literal(&format!("set {}", attr.name)),
        ));
      }
      out.push_str(&format!(
        "      rt.define_accessor_property_str(\n        {proto_var},\n        {attr_lit},\n        Value::Object(get),\n        set,\n        AccessorPropertyAttributes::ATTRIBUTE,\n      )?;\n    }}\n  }}\n",
        proto_var = proto_var,
        attr_lit = rust_string_literal(&attr.name),
      ));
    }
    // Static methods.
    for (op_name, overloads) in &iface.static_operations {
      let length = overloads
        .iter()
        .map(|sig| required_arg_count(&sig.arguments))
        .min()
        .unwrap_or(0) as u32;
      out.push_str(&format!(
        "  {{\n    let key = rt.property_key({name_lit})?;\n    if rt.scope.heap().object_get_own_property({ctor_var}, &key)?.is_none() {{\n      let func = rt.alloc_native_function({func}, None, {name_lit}, {length})?;\n      rt.define_data_property_str(\n        {ctor_var},\n        {name_lit},\n        Value::Object(func),\n        DataPropertyAttributes::METHOD,\n      )?;\n    }}\n  }}\n",
        func = op_wrapper_fn_name(&iface.name, op_name),
        name_lit = rust_string_literal(op_name),
        ctor_var = ctor_var,
        length = length,
      ));
    }

    // Static attributes.
    for attr in iface.static_attributes.values() {
      out.push_str(&format!(
        "  {{\n    let key = rt.property_key({attr_lit})?;\n    if rt.scope.heap().object_get_own_property({ctor_var}, &key)?.is_none() {{\n      let get = rt.alloc_native_function({getter}, None, {get_name_lit}, 0)?;\n",
        ctor_var = ctor_var,
        attr_lit = rust_string_literal(&attr.name),
        getter = attr_getter_fn_name(&iface.name, &attr.name, true),
        get_name_lit = rust_string_literal(&format!("get {}", attr.name)),
      ));
      if attr.readonly {
        out.push_str("      let set = Value::Undefined;\n");
      } else {
        out.push_str(&format!(
          "      let set = Value::Object(rt.alloc_native_function({setter}, None, {set_name_lit}, 1)?);\n",
          setter = attr_setter_fn_name(&iface.name, &attr.name, true),
          set_name_lit = rust_string_literal(&format!("set {}", attr.name)),
        ));
      }
      out.push_str(&format!(
        "      rt.define_accessor_property_str(\n        {ctor_var},\n        {attr_lit},\n        Value::Object(get),\n        set,\n        AccessorPropertyAttributes::ATTRIBUTE,\n      )?;\n    }}\n  }}\n",
        ctor_var = ctor_var,
        attr_lit = rust_string_literal(&attr.name),
      ));
    }

    // Constants.
    for constant in iface.constants.values() {
      write_constant_define_vmjs(&mut out, &ctor_var, &proto_var, constant);
    }

    out.push_str("  Ok(())\n");
    out.push_str("}\n\n");
  }

  // Convenience aggregator (maintains the old entrypoint name).
  out.push_str(&format!(
    "pub fn {install_fn_name}(\n  vm: &mut Vm,\n  heap: &mut Heap,\n  realm: &Realm,\n) -> Result<(), VmError> {{\n",
  ));
  for iface in install_order {
    let install = interface_installer_by_name
      .get(&iface.name)
      .with_context(|| format!("missing installer name for `{}`", iface.name))?;
    out.push_str(&format!(
      "  {install}(vm, heap, realm)?;\n",
      install = install
    ));
  }
  out.push_str("  Ok(())\n");
  out.push_str("}\n");

  // Avoid unused-import warnings in generated modules that don't use all helper symbols.
  let needs_binding_value = out.contains("BindingValue::");
  let needs_to_int32 = out.contains("to_int32_f64(");
  let needs_to_uint32 = out.contains("to_uint32_f64(");
  let mut imports: Vec<&str> = Vec::new();
  if needs_accessor_property_attributes {
    imports.push("AccessorPropertyAttributes");
  }
  if needs_binding_value {
    imports.push("BindingValue");
  }
  imports.push("BindingsRuntime");
  imports.push("DataPropertyAttributes");
  if needs_to_int32 {
    imports.push("to_int32_f64");
  }
  if needs_to_uint32 {
    imports.push("to_uint32_f64");
  }
  let import_line = format!(
    "use webidl_vm_js::bindings_runtime::{{{}}};\n",
    imports.join(", ")
  );
  out = out.replace(
    "use webidl_vm_js::bindings_runtime::{AccessorPropertyAttributes, BindingValue, BindingsRuntime, DataPropertyAttributes, to_int32_f64, to_uint32_f64};\n",
    &import_line,
  );

  Ok((out, interface_installers))
}

fn order_selected_interfaces_by_inheritance_vmjs<'a>(
  selected: &'a BTreeMap<String, SelectedInterface>,
  is_global_iface: &impl Fn(&str) -> bool,
) -> Result<Vec<&'a SelectedInterface>> {
  let mut ordered: Vec<&'a SelectedInterface> = Vec::new();
  let mut visiting: BTreeSet<String> = BTreeSet::new();
  let mut visited: BTreeSet<String> = BTreeSet::new();
  let mut globals: Vec<&'a SelectedInterface> = Vec::new();

  fn visit<'a>(
    name: &str,
    selected: &'a BTreeMap<String, SelectedInterface>,
    is_global_iface: &impl Fn(&str) -> bool,
    visiting: &mut BTreeSet<String>,
    visited: &mut BTreeSet<String>,
    ordered: &mut Vec<&'a SelectedInterface>,
  ) -> Result<()> {
    if visited.contains(name) {
      return Ok(());
    }
    if !visiting.insert(name.to_string()) {
      bail!("cycle detected in WebIDL interface inheritance graph (at `{name}`)");
    }

    let iface = selected
      .get(name)
      .with_context(|| format!("missing selected interface `{name}` during sort"))?;
    if !is_global_iface(&iface.name) {
      if let Some(parent) = iface.inherits.as_deref() {
        if selected.contains_key(parent) && !is_global_iface(parent) {
          visit(
            parent,
            selected,
            is_global_iface,
            visiting,
            visited,
            ordered,
          )?;
        }
      }
    }

    visiting.remove(name);
    visited.insert(name.to_string());
    ordered.push(iface);
    Ok(())
  }

  for name in selected.keys() {
    if is_global_iface(name) {
      globals.push(&selected[name]);
    } else {
      visit(
        name,
        selected,
        is_global_iface,
        &mut visiting,
        &mut visited,
        &mut ordered,
      )?;
    }
  }

  ordered.extend(globals);
  Ok(ordered)
}

fn write_constant_define_vmjs(
  out: &mut String,
  ctor_var: &str,
  proto_var: &str,
  constant: &ConstantSig,
) {
  match &constant.value {
    IdlLiteral::String(s) => {
      out.push_str(&format!(
        "  {{\n    let key = rt.property_key({name_lit})?;\n    let install_ctor = rt.scope.heap().object_get_own_property({ctor_var}, &key)?.is_none();\n    let install_proto = rt.scope.heap().object_get_own_property({proto_var}, &key)?.is_none();\n    if install_ctor || install_proto {{\n      let value = Value::String(rt.alloc_string({value_lit})?);\n      let value = rt.scope.push_root(value)?;\n      if install_ctor {{\n        rt.define_data_property_str({ctor_var}, {name_lit}, value, DataPropertyAttributes::CONST)?;\n      }}\n      if install_proto {{\n        rt.define_data_property_str({proto_var}, {name_lit}, value, DataPropertyAttributes::CONST)?;\n      }}\n    }}\n  }}\n",
        ctor_var = ctor_var,
        proto_var = proto_var,
        name_lit = rust_string_literal(&constant.name),
        value_lit = rust_string_literal(s),
      ));
    }
    _ => {
      let expr = emit_constant_value_expr_vmjs(&constant.value);
      out.push_str(&format!(
        "  {{\n    let key = rt.property_key({name_lit})?;\n    if rt.scope.heap().object_get_own_property({ctor_var}, &key)?.is_none() {{\n      rt.define_data_property_str({ctor_var}, {name_lit}, {expr}, DataPropertyAttributes::CONST)?;\n    }}\n    if rt.scope.heap().object_get_own_property({proto_var}, &key)?.is_none() {{\n      rt.define_data_property_str({proto_var}, {name_lit}, {expr}, DataPropertyAttributes::CONST)?;\n    }}\n  }}\n",
        ctor_var = ctor_var,
        proto_var = proto_var,
        name_lit = rust_string_literal(&constant.name),
        expr = expr,
      ));
    }
  }
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

          iterable = Some(IterableInfo {
            async_: *async_,
            key_type: key_ir,
            value_type: value_ir,
          });

          // WebIDL iterable declarations synthesize default operations.
          // These methods are not explicitly listed in spec IDL sources; they are implied by the
          // `iterable<>`/`async iterable<>` declaration. Emit them even in allowlist mode so
          // interfaces like URLSearchParams are spec-shaped by default.
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
    IdlType::Annotated { inner, .. } => render_idl_type(inner),
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
      format!(
        "record<{}, {}>",
        render_idl_type(key),
        render_idl_type(value)
      )
    }
  }
}

#[derive(Debug, Clone, Default)]
struct ReferencedTypeContextEntries {
  dictionaries: BTreeSet<String>,
  enums: BTreeSet<String>,
  typedefs: BTreeSet<String>,
}

fn collect_referenced_type_context_entries(
  ctx: &TypeContext,
  interfaces: &BTreeMap<String, SelectedInterface>,
) -> ReferencedTypeContextEntries {
  // Seed with named types referenced directly by selected signatures.
  let mut pending = Vec::<String>::new();
  {
    let mut named = BTreeSet::<String>::new();
    for iface in interfaces.values() {
      for ctor in &iface.constructors {
        for arg in &ctor.arguments {
          collect_named_types(&arg.type_, &mut named);
        }
      }
      for overloads in iface
        .operations
        .values()
        .chain(iface.static_operations.values())
      {
        for sig in overloads {
          collect_named_types(&sig.return_type, &mut named);
          for arg in &sig.arguments {
            collect_named_types(&arg.type_, &mut named);
          }
        }
      }
      for attr in iface
        .attributes
        .values()
        .chain(iface.static_attributes.values())
      {
        collect_named_types(&attr.type_, &mut named);
      }
    }
    pending.extend(named);
  }

  let mut out = ReferencedTypeContextEntries::default();

  while let Some(name) = pending.pop() {
    if ctx.dictionaries.contains_key(&name) {
      if !out.dictionaries.insert(name.clone()) {
        continue;
      }

      let dict = &ctx.dictionaries[&name];
      if let Some(parent) = &dict.inherits {
        pending.push(parent.clone());
      }
      for member in &dict.members {
        let mut names = BTreeSet::<String>::new();
        collect_named_types_ir(&member.ty, &mut names);
        pending.extend(names);
      }
      continue;
    }

    if ctx.enums.contains_key(&name) {
      out.enums.insert(name);
      continue;
    }

    if let Some(td) = ctx.typedefs.get(&name) {
      if !out.typedefs.insert(name.clone()) {
        continue;
      }
      let mut names = BTreeSet::<String>::new();
      collect_named_types_ir(td, &mut names);
      pending.extend(names);
      continue;
    }
  }

  out
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
    IdlType::Annotated { inner, .. } => collect_named_types(inner, out),
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

  for webidl::ir::DictionaryMemberSchema {
    name: member_name,
    required,
    ty,
    default,
  } in members
  {
    let member_ty = expand_typedefs_in_type(type_ctx, &ty)?;
    let conversion_expr =
      emit_conversion_expr_ir(resolved, type_ctx, &member_ty, "js_member_value")?;

    out.push_str("  {\n");
    out.push_str(&format!(
      "    let js_member_value = if is_missing {{\n      rt.js_undefined()\n    }} else {{\n      let key = rt.property_key({name_lit})?;\n      rt.get(host, value, key)?\n    }};\n",
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
      let default_expr =
        emit_default_value_ir(type_ctx, &member_ty, &default).with_context(|| {
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
  legacy_treat_non_object_as_null: bool,
}
fn write_dictionary_converter_vmjs(
  out: &mut String,
  resolved: &ResolvedWebIdlWorld,
  dict: &crate::webidl::resolve::ResolvedDictionary,
) {
  let fn_name = format!("js_to_dict_{}", to_snake_ident(&dict.name));
  out.push_str(&format!(
    "#[allow(dead_code)]\nfn {fn_name}(\n  rt: &mut BindingsRuntime<'_>,\n  host: &mut dyn VmHost,\n  hooks: &mut dyn VmHostHooks,\n  value: Value,\n) -> Result<Value, VmError>\n{{\n",
  ));
  out.push_str("  let _ = (&mut *host, &mut *hooks);\n");
  out.push_str("  if matches!(value, Value::Undefined | Value::Null) {\n");
  out.push_str("    let obj = rt.alloc_object()?;\n");
  out.push_str("    return Ok(Value::Object(obj));\n");
  out.push_str("  }\n");
  out.push_str("  let Value::Object(input) = value else {\n");
  out.push_str(&format!(
    "    return Err(rt.throw_type_error(\"expected object for dictionary {}\"));\n",
    dict.name
  ));
  out.push_str("  };\n");
  out.push_str("  rt.scope.push_root(Value::Object(input))?;\n");
  out.push_str("  let out_obj = rt.alloc_object()?;\n");

  for member in resolved.flattened_dictionary_members(&dict.name) {
    let Some((ty, member_name)) = parse_dictionary_member_type(&member.raw) else {
      continue;
    };
    out.push_str(&format!(
      "  {{\n    let key = rt.property_key({name_lit})?;\n    let v = rt.vm.get(&mut rt.scope, input, key)?;\n    if !matches!(v, Value::Undefined) {{\n",
      name_lit = rust_string_literal(&member_name)
    ));
    out.push_str(&format!(
      "      let converted = {};\n",
      emit_conversion_expr_vmjs(resolved, &ty, "v", true)
    ));
    out.push_str(&format!(
      "      rt.define_data_property_str(out_obj, {name_lit}, converted, DataPropertyAttributes::new(true, true, true))?;\n",
      name_lit = rust_string_literal(&member_name)
    ));
    out.push_str("    }\n  }\n");
  }

  out.push_str("  Ok(Value::Object(out_obj))\n");
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

fn emit_conversion_expr_ir(
  resolved: &ResolvedWebIdlWorld,
  type_ctx: &TypeContext,
  ty: &IrIdlType,
  value_ident: &str,
) -> Result<String> {
  emit_conversion_expr_ir_inner(
    resolved,
    type_ctx,
    ty,
    value_ident,
    IrConversionState::default(),
  )
}

fn emit_conversion_expr_ir_inner(
  resolved: &ResolvedWebIdlWorld,
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
            IrTypeAnnotation::LegacyTreatNonObjectAsNull => next.legacy_treat_non_object_as_null = true,
            _ => {}
          }
        }
        if next.clamp && next.enforce_range {
          bail!("[Clamp] and [EnforceRange] cannot both apply to the same type");
        }
        emit_conversion_expr_ir_inner(resolved, type_ctx, inner, value_ident, next)
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
          "BindingValue::Number(conversions::to_byte(rt, host, {value_ident}, {int_attrs})? as f64)",
          value_ident = value_ident,
          int_attrs = int_attrs.clone(),
        )),
        IrNumericType::Octet => Ok(format!(
          "BindingValue::Number(conversions::to_octet(rt, host, {value_ident}, {int_attrs})? as f64)",
          value_ident = value_ident,
          int_attrs = int_attrs.clone(),
        )),
        IrNumericType::Short => Ok(format!(
          "BindingValue::Number(conversions::to_short(rt, host, {value_ident}, {int_attrs})? as f64)",
          value_ident = value_ident,
          int_attrs = int_attrs.clone(),
        )),
        IrNumericType::UnsignedShort => Ok(format!(
          "BindingValue::Number(conversions::to_unsigned_short(rt, host, {value_ident}, {int_attrs})? as f64)",
          value_ident = value_ident,
          int_attrs = int_attrs.clone(),
        )),
        IrNumericType::Long => Ok(format!(
          "BindingValue::Number(conversions::to_long(rt, host, {value_ident}, {int_attrs})? as f64)",
          value_ident = value_ident,
          int_attrs = int_attrs.clone(),
        )),
        IrNumericType::UnsignedLong => Ok(format!(
          "BindingValue::Number(conversions::to_unsigned_long(rt, host, {value_ident}, {int_attrs})? as f64)",
          value_ident = value_ident,
          int_attrs = int_attrs.clone(),
        )),
        IrNumericType::LongLong => Ok(format!(
          "BindingValue::Number(conversions::to_long_long(rt, host, {value_ident}, {int_attrs})? as f64)",
          value_ident = value_ident,
          int_attrs = int_attrs.clone(),
        )),
        IrNumericType::UnsignedLongLong => Ok(format!(
          "BindingValue::Number(conversions::to_unsigned_long_long(rt, host, {value_ident}, {int_attrs})? as f64)",
          value_ident = value_ident,
          int_attrs = int_attrs,
        )),
        IrNumericType::Float => {
          if state.clamp || state.enforce_range {
            bail!("[Clamp]/[EnforceRange] annotations only apply to integer numeric types");
          }
          Ok(format!(
            "BindingValue::Number(conversions::to_float(rt, host, {value_ident})? as f64)",
            value_ident = value_ident
          ))
        }
        IrNumericType::UnrestrictedFloat => {
          if state.clamp || state.enforce_range {
            bail!("[Clamp]/[EnforceRange] annotations only apply to integer numeric types");
          }
          Ok(format!(
            "BindingValue::Number(conversions::to_unrestricted_float(rt, host, {value_ident})? as f64)",
            value_ident = value_ident
          ))
        }
        IrNumericType::Double => {
          if state.clamp || state.enforce_range {
            bail!("[Clamp]/[EnforceRange] annotations only apply to integer numeric types");
          }
          Ok(format!(
            "BindingValue::Number(conversions::to_double(rt, host, {value_ident})?)",
            value_ident = value_ident
          ))
        }
        IrNumericType::UnrestrictedDouble => {
          if state.clamp || state.enforce_range {
            bail!("[Clamp]/[EnforceRange] annotations only apply to integer numeric types");
          }
          Ok(format!(
            "BindingValue::Number(conversions::to_unrestricted_double(rt, host, {value_ident})?)",
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
        "{{ let s = rt.to_string(host, {value_ident})?; BindingValue::String(rt.js_string_to_rust_string(s)?) }}",
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
        NamedTypeKind::Enum => {
          let allowed = resolved
            .enums
            .get(&named.name)
            .map(|e| e.values.as_slice())
            .unwrap_or_default();
          let allowed_lit = allowed
            .iter()
            .map(|v| rust_string_literal(v))
            .collect::<Vec<_>>()
            .join(", ");
          Ok(format!(
            "BindingValue::String(conversions::to_enum::<Host, R>(rt, host, {value_ident}, {enum_name}, &[{allowed_lit}])?)",
            value_ident = value_ident,
            enum_name = rust_string_literal(&named.name),
            allowed_lit = allowed_lit,
          ))
        }
        NamedTypeKind::Typedef => {
          if let Some(inner) = type_ctx.typedefs.get(&named.name) {
            emit_conversion_expr_ir_inner(resolved, type_ctx, inner, value_ident, state)
          } else {
            Ok(format!("BindingValue::Object({value_ident})"))
          }
        }
        NamedTypeKind::CallbackFunction => {
          if state.legacy_treat_non_object_as_null {
            Ok(format!(
              "if !rt.is_object({value_ident}) {{ BindingValue::Null }} else {{ BindingValue::Callback(rt.root_callback_function({value_ident})?) }}",
              value_ident = value_ident
            ))
          } else {
            Ok(format!(
              "BindingValue::Callback(rt.root_callback_function({value_ident})?)",
              value_ident = value_ident
            ))
          }
        }
        NamedTypeKind::CallbackInterface => Ok(format!(
          "BindingValue::Callback(rt.root_callback_interface({value_ident})?)",
          value_ident = value_ident
        )),
        _ => Ok(format!("BindingValue::Object({value_ident})")),
      }
    }

    IrIdlType::Nullable(inner) => Ok(format!(
      "if rt.is_null({value_ident}) || rt.is_undefined({value_ident}) {{ BindingValue::Null }} else {{ {inner_expr} }}",
      value_ident = value_ident,
      inner_expr = emit_conversion_expr_ir_inner(resolved, type_ctx, inner, value_ident, state)?,
    )),

    IrIdlType::Sequence(elem) => {
      if state.clamp || state.enforce_range || state.legacy_null_to_empty_string {
        bail!("unexpected type annotations on `sequence`");
      }
      emit_iterable_list_conversion_expr_ir(resolved, type_ctx, elem, value_ident, "sequence", "Sequence")
    }
    IrIdlType::FrozenArray(elem) => {
      if state.clamp || state.enforce_range || state.legacy_null_to_empty_string {
        bail!("unexpected type annotations on `FrozenArray`");
      }
      emit_iterable_list_conversion_expr_ir(
        resolved,
        type_ctx,
        elem,
        value_ident,
        "FrozenArray",
        "FrozenArray",
      )
    }

    IrIdlType::Union(members) => {
      if state.clamp || state.enforce_range || state.legacy_null_to_empty_string {
        bail!("unexpected type annotations on union type");
      }
      emit_union_conversion_expr_ir(resolved, type_ctx, members, value_ident)
    }
    IrIdlType::Record(_key, value) => {
      if state.clamp || state.enforce_range || state.legacy_null_to_empty_string {
        bail!("unexpected type annotations on record type");
      }
      let value_expr = emit_conversion_expr_ir_inner(
        resolved,
        type_ctx,
        value,
        "v",
        IrConversionState::default(),
      )?;
      Ok(format!(
        "conversions::to_record(rt, host, {value_ident}, |rt, host, v| Ok({value_expr}))?",
        value_ident = value_ident,
        value_expr = value_expr
      ))
    }
    IrIdlType::AsyncSequence(_) | IrIdlType::Promise(_) => {
      if state.clamp || state.enforce_range || state.legacy_null_to_empty_string {
        bail!("unexpected type annotations on non-primitive type");
      }
      Ok(format!("BindingValue::Object({value_ident})"))
    }
  }
}

fn emit_union_conversion_expr_ir(
  resolved: &ResolvedWebIdlWorld,
  type_ctx: &TypeContext,
  members: &[IrIdlType],
  value_ident: &str,
) -> Result<String> {
  let mut expanded_members: Vec<IrIdlType> = Vec::with_capacity(members.len());
  for m in members {
    expanded_members.push(expand_typedefs_in_type(type_ctx, m)?);
  }

  let mut has_undefined = false;
  let mut has_nullable = false;
  let mut has_any = false;
  let mut has_object = false;

  let mut sequence_member: Option<&IrIdlType> = None;
  let mut dict_member: Option<&String> = None;
  let mut record_member: Option<&IrIdlType> = None;
  let mut callback_function_member: Option<&IrIdlType> = None;
  let mut callback_interface_member: Option<&IrIdlType> = None;
  let mut interface_like: Vec<&String> = Vec::new();
  let mut boolean_member: Option<&IrIdlType> = None;
  let mut numeric_member: Option<&IrIdlType> = None;
  let mut string_member: Option<&IrIdlType> = None;
  let mut bigint_member: Option<&IrIdlType> = None;
  let mut symbol_member: Option<&IrIdlType> = None;

  fn strip<'a>(ty: &'a IrIdlType, has_nullable: &mut bool) -> &'a IrIdlType {
    let mut t = ty;
    loop {
      match t {
        IrIdlType::Annotated { inner, .. } => t = inner,
        IrIdlType::Nullable(inner) => {
          *has_nullable = true;
          t = inner;
        }
        _ => return t,
      }
    }
  }

  for member in &expanded_members {
    let inner = strip(member, &mut has_nullable);
    match inner {
      IrIdlType::Undefined => has_undefined = true,
      IrIdlType::Any => has_any = true,
      IrIdlType::Object => has_object = true,
      IrIdlType::Boolean => {
        if boolean_member.is_none() {
          boolean_member = Some(member);
        }
      }
      IrIdlType::Numeric(_) => {
        if numeric_member.is_none() {
          numeric_member = Some(member);
        }
      }
      IrIdlType::BigInt => {
        if bigint_member.is_none() {
          bigint_member = Some(member);
        }
      }
      IrIdlType::String(_) => {
        if string_member.is_none() {
          string_member = Some(member);
        }
      }
      IrIdlType::Symbol => {
        if symbol_member.is_none() {
          symbol_member = Some(member);
        }
      }
      IrIdlType::Sequence(_) | IrIdlType::FrozenArray(_) => {
        if sequence_member.is_none() {
          sequence_member = Some(member);
        }
      }
      IrIdlType::Record(_, _) => {
        if record_member.is_none() {
          record_member = Some(member);
        }
      }
      IrIdlType::Named(named) => match named.kind {
        NamedTypeKind::Dictionary => {
          if dict_member.is_none() {
            dict_member = Some(&named.name);
          }
        }
        NamedTypeKind::Enum => {
          if string_member.is_none() {
            string_member = Some(member);
          }
        }
        NamedTypeKind::CallbackFunction => {
          if callback_function_member.is_none() {
            callback_function_member = Some(member);
          }
        }
        NamedTypeKind::CallbackInterface => {
          if callback_interface_member.is_none() {
            callback_interface_member = Some(member);
          }
        }
        NamedTypeKind::Interface => {
          interface_like.push(&named.name);
        }
        NamedTypeKind::Typedef | NamedTypeKind::Unresolved => {}
      },
      IrIdlType::Promise(_) | IrIdlType::AsyncSequence(_) | IrIdlType::Union(_) => {}
      IrIdlType::Nullable(_) | IrIdlType::Annotated { .. } => {}
    }
  }

  let wrap = |member_type: &str, expr: String| -> String {
    format!(
      "BindingValue::Union {{ member_type: {member_type_lit}.to_string(), value: Box::new({expr}) }}",
      member_type_lit = rust_string_literal(member_type),
      expr = expr
    )
  };

  let dict_expr = dict_member.map(|dict| {
    wrap(
      dict,
      format!(
        "js_to_dict_{}::<Host, R>(rt, host, v)?",
        to_snake_ident(dict)
      ),
    )
  });
  let seq_expr = if let Some(ty) = sequence_member {
    let expr =
      emit_conversion_expr_ir_inner(resolved, type_ctx, ty, "v", IrConversionState::default())?;
    Some(wrap(&ty.to_string(), expr))
  } else {
    None
  };
  let record_expr = if let Some(ty) = record_member {
    let expr =
      emit_conversion_expr_ir_inner(resolved, type_ctx, ty, "v", IrConversionState::default())?;
    Some(wrap(&ty.to_string(), expr))
  } else {
    None
  };
  let callback_expr = if let Some(ty) = callback_function_member {
    let expr =
      emit_conversion_expr_ir_inner(resolved, type_ctx, ty, "v", IrConversionState::default())?;
    Some(wrap(&ty.to_string(), expr))
  } else {
    None
  };
  let callback_iface_expr = if let Some(ty) = callback_interface_member {
    let expr =
      emit_conversion_expr_ir_inner(resolved, type_ctx, ty, "v", IrConversionState::default())?;
    Some(wrap(&ty.to_string(), expr))
  } else {
    None
  };
  let boolean_expr = if let Some(ty) = boolean_member {
    let expr =
      emit_conversion_expr_ir_inner(resolved, type_ctx, ty, "v", IrConversionState::default())?;
    Some(wrap(&ty.to_string(), expr))
  } else {
    None
  };
  let numeric_expr = if let Some(ty) = numeric_member {
    let expr =
      emit_conversion_expr_ir_inner(resolved, type_ctx, ty, "v", IrConversionState::default())?;
    Some(wrap(&ty.to_string(), expr))
  } else {
    None
  };
  let string_expr = if let Some(ty) = string_member {
    let expr =
      emit_conversion_expr_ir_inner(resolved, type_ctx, ty, "v", IrConversionState::default())?;
    Some(wrap(&ty.to_string(), expr))
  } else {
    None
  };
  let bigint_expr = bigint_member.map(|_| wrap("bigint", "BindingValue::Object(v)".to_string()));
  let symbol_expr = symbol_member.map(|_| wrap("symbol", "BindingValue::Object(v)".to_string()));

  let any_expr = wrap("any", "BindingValue::Object(v)".to_string());
  let object_expr = wrap("object", "BindingValue::Object(v)".to_string());
  let null_expr = wrap("null", "BindingValue::Null".to_string());
  let undefined_expr = wrap("undefined", "BindingValue::Undefined".to_string());

  let mut out = String::new();
  out.push_str("{\n");
  out.push_str(&format!(
    "  let v = {value_ident};\n",
    value_ident = value_ident
  ));

  if has_undefined {
    out.push_str("  if rt.is_undefined(v) {\n    ");
    out.push_str(&undefined_expr);
    out.push_str("\n  }");
  } else {
    out.push_str("  if false {\n    BindingValue::Undefined\n  }");
  }

  if let Some(dict_expr) = &dict_expr {
    out.push_str(" else if rt.is_null(v) || rt.is_undefined(v) {\n    ");
    out.push_str(dict_expr);
    out.push_str("\n  }");
  }

  if has_nullable {
    out.push_str(" else if rt.is_null(v) || rt.is_undefined(v) {\n    ");
    out.push_str(&null_expr);
    out.push_str("\n  }");
  }

  for iface in &interface_like {
    let iface_expr = wrap(iface, "BindingValue::Object(v)".to_string());
    out.push_str(&format!(
      " else if rt.is_platform_object(v) && rt.implements_interface(v, crate::js::webidl::interface_id_from_name({iface_lit})) {{\n    {iface_expr}\n  }}",
      iface_lit = rust_string_literal(iface),
      iface_expr = iface_expr
    ));
  }

  if let Some(callback_expr) = &callback_expr {
    out.push_str(" else if rt.is_callable(v) {\n    ");
    out.push_str(callback_expr);
    out.push_str("\n  }");
  }

  out.push_str(" else if rt.is_object(v) {\n");
  if let Some(seq_expr) = &seq_expr {
    out.push_str(
      "    let has_iter = {\n      let iterator_key = rt.symbol_iterator()?;\n      rt.get_method(host, v, iterator_key)?.is_some() || rt.is_array(v)?\n    };\n",
    );
    out.push_str("    if has_iter {\n      ");
    out.push_str(seq_expr);
    out.push_str("\n    }");

    if let Some(dict_expr) = &dict_expr {
      out.push_str(" else {\n      ");
      out.push_str(dict_expr);
      out.push_str("\n    }");
    } else if let Some(record_expr) = &record_expr {
      out.push_str(" else {\n      ");
      out.push_str(record_expr);
      out.push_str("\n    }");
    } else if let Some(callback_iface_expr) = &callback_iface_expr {
      out.push_str(" else {\n      ");
      out.push_str(callback_iface_expr);
      out.push_str("\n    }");
    } else if has_object || has_any {
      out.push_str(" else {\n      ");
      out.push_str(if has_object { &object_expr } else { &any_expr });
      out.push_str("\n    }");
    } else {
      out.push_str(" else {\n      return Err(rt.throw_type_error(\"Value is not a valid union type\"));\n    }");
    }
    out.push_str("\n  }");
  } else if let Some(dict_expr) = &dict_expr {
    out.push_str("    ");
    out.push_str(dict_expr);
    out.push_str("\n  }");
  } else if let Some(record_expr) = &record_expr {
    out.push_str("    ");
    out.push_str(record_expr);
    out.push_str("\n  }");
  } else if let Some(callback_iface_expr) = &callback_iface_expr {
    out.push_str("    ");
    out.push_str(callback_iface_expr);
    out.push_str("\n  }");
  } else if has_object || has_any {
    out.push_str("    ");
    out.push_str(if has_object { &object_expr } else { &any_expr });
    out.push_str("\n  }");
  } else {
    out.push_str("    return Err(rt.throw_type_error(\"Value is not a valid union type\"));\n  }");
  }

  if let Some(boolean_expr) = &boolean_expr {
    out.push_str(" else if rt.is_boolean(v) {\n    ");
    out.push_str(boolean_expr);
    out.push_str("\n  }");
  }
  if let Some(numeric_expr) = &numeric_expr {
    out.push_str(" else if rt.is_number(v) {\n    ");
    out.push_str(numeric_expr);
    out.push_str("\n  }");
  }
  if let Some(string_expr) = &string_expr {
    out.push_str(" else if rt.is_string(v) || rt.is_string_object(v) {\n    ");
    out.push_str(string_expr);
    out.push_str("\n  }");
  }
  if let Some(bigint_expr) = &bigint_expr {
    out.push_str(" else if rt.is_bigint(v) {\n    ");
    out.push_str(bigint_expr);
    out.push_str("\n  }");
  }
  if let Some(symbol_expr) = &symbol_expr {
    out.push_str(" else if rt.is_symbol(v) {\n    ");
    out.push_str(symbol_expr);
    out.push_str("\n  }");
  }

  out.push_str(" else {\n    ");
  if let Some(string_expr) = &string_expr {
    out.push_str(string_expr);
    out.push_str("\n  }\n");
  } else if let Some(numeric_expr) = &numeric_expr {
    out.push_str(numeric_expr);
    out.push_str("\n  }\n");
  } else if let Some(boolean_expr) = &boolean_expr {
    out.push_str(boolean_expr);
    out.push_str("\n  }\n");
  } else if has_any {
    out.push_str(&any_expr);
    out.push_str("\n  }\n");
  } else {
    out.push_str("return Err(rt.throw_type_error(\"Value is not a valid union type\"));\n  }\n");
  }

  out.push_str("}\n");
  Ok(out)
}

fn emit_iterable_list_conversion_expr_ir(
  resolved: &ResolvedWebIdlWorld,
  type_ctx: &TypeContext,
  elem_ty: &IrIdlType,
  value_ident: &str,
  kind_label: &str,
  out_variant: &str,
) -> Result<String> {
  let elem_expr = emit_conversion_expr_ir_inner(
    resolved,
    type_ctx,
    elem_ty,
    "next",
    IrConversionState::default(),
  )?;
  Ok(format!(
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
  ))
}

fn emit_default_value_ir(
  type_ctx: &TypeContext,
  ty: &IrIdlType,
  default: &IrDefaultValue,
) -> Result<String> {
  let evaluated =
    webidl::ir::eval_default_value(ty, default, type_ctx).map_err(|e| anyhow::anyhow!("{e}"))?;
  Ok(emit_binding_value_expr_from_webidl_value(&evaluated))
}

fn emit_binding_value_expr_from_webidl_value(v: &webidl::ir::WebIdlValue) -> String {
  match v {
    webidl::ir::WebIdlValue::Undefined => "BindingValue::Undefined".to_string(),
    webidl::ir::WebIdlValue::Null => "BindingValue::Null".to_string(),
    webidl::ir::WebIdlValue::Boolean(b) => {
      format!("BindingValue::Bool({})", if *b { "true" } else { "false" })
    }

    webidl::ir::WebIdlValue::Byte(n) => {
      format!("BindingValue::Number({})", emit_f64_literal(*n as f64))
    }
    webidl::ir::WebIdlValue::Octet(n) => {
      format!("BindingValue::Number({})", emit_f64_literal(*n as f64))
    }
    webidl::ir::WebIdlValue::Short(n) => {
      format!("BindingValue::Number({})", emit_f64_literal(*n as f64))
    }
    webidl::ir::WebIdlValue::UnsignedShort(n) => {
      format!("BindingValue::Number({})", emit_f64_literal(*n as f64))
    }
    webidl::ir::WebIdlValue::Long(n) => {
      format!("BindingValue::Number({})", emit_f64_literal(*n as f64))
    }
    webidl::ir::WebIdlValue::UnsignedLong(n) => {
      format!("BindingValue::Number({})", emit_f64_literal(*n as f64))
    }
    webidl::ir::WebIdlValue::LongLong(n) => {
      format!("BindingValue::Number({})", emit_f64_literal(*n as f64))
    }
    webidl::ir::WebIdlValue::UnsignedLongLong(n) => {
      format!("BindingValue::Number({})", emit_f64_literal(*n as f64))
    }
    webidl::ir::WebIdlValue::Float(n) | webidl::ir::WebIdlValue::UnrestrictedFloat(n) => {
      format!("BindingValue::Number({})", emit_f64_literal(*n as f64))
    }
    webidl::ir::WebIdlValue::Double(n) | webidl::ir::WebIdlValue::UnrestrictedDouble(n) => {
      format!("BindingValue::Number({})", emit_f64_literal(*n))
    }

    webidl::ir::WebIdlValue::String(s) | webidl::ir::WebIdlValue::Enum(s) => {
      format!(
        "BindingValue::String({}.to_string())",
        rust_string_literal(s)
      )
    }

    webidl::ir::WebIdlValue::Sequence { values, .. } => {
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

    webidl::ir::WebIdlValue::Record { entries, .. } => {
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

    webidl::ir::WebIdlValue::Dictionary { members, .. } => {
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

    webidl::ir::WebIdlValue::Union { value, .. } => {
      emit_binding_value_expr_from_webidl_value(value)
    }
    webidl::ir::WebIdlValue::PlatformObject(_) => "BindingValue::Undefined".to_string(),
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
    IdlType::Annotated { ext_attrs, inner } => {
      let mut out = String::new();
      out.push('[');
      for (idx, attr) in ext_attrs.iter().enumerate() {
        if idx != 0 {
          out.push_str(", ");
        }
        out.push_str(&attr.name);
        if let Some(v) = &attr.value {
          out.push_str(" = ");
          out.push_str(v);
        }
      }
      out.push_str("] ");
      out.push_str(&ast_idl_type_to_webidl_ir_src(inner));
      out
    }
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

fn idl_literal_to_webidl_ir_default_value(lit: &IdlLiteral) -> Option<webidl::ir::DefaultValue> {
  use webidl::ir::{DefaultValue, NumericLiteral};
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

fn render_webidl_ir_default_value(value: &webidl::ir::DefaultValue) -> String {
  use webidl::ir::{DefaultValue, NumericLiteral};
  match value {
    DefaultValue::Boolean(b) => format!(
      "DefaultValue::Boolean({})",
      if *b { "true" } else { "false" }
    ),
    DefaultValue::Null => "DefaultValue::Null".to_string(),
    DefaultValue::Undefined => "DefaultValue::Undefined".to_string(),
    DefaultValue::Number(n) => match n {
      NumericLiteral::Integer(s) => format!(
        "DefaultValue::Number(NumericLiteral::Integer({}))",
        format!("{}.to_string()", rust_string_literal(s))
      ),
      NumericLiteral::Decimal(s) => format!(
        "DefaultValue::Number(NumericLiteral::Decimal({}))",
        format!("{}.to_string()", rust_string_literal(s))
      ),
      NumericLiteral::Infinity { negative } => format!(
        "DefaultValue::Number(NumericLiteral::Infinity {{ negative: {} }})",
        if *negative { "true" } else { "false" }
      ),
      NumericLiteral::NaN => "DefaultValue::Number(NumericLiteral::NaN)".to_string(),
    },
    DefaultValue::String(s) => {
      format!(
        "DefaultValue::String({}.to_string())",
        rust_string_literal(s)
      )
    }
    DefaultValue::EmptySequence => "DefaultValue::EmptySequence".to_string(),
    DefaultValue::EmptyDictionary => "DefaultValue::EmptyDictionary".to_string(),
  }
}

fn render_webidl_ir_type(ty: &webidl::ir::IdlType) -> String {
  use webidl::ir::{IdlType, NamedType, NamedTypeKind, NumericType, StringType, TypeAnnotation};

  match ty {
    IdlType::Any => "IdlType::Any".to_string(),
    IdlType::Undefined => "IdlType::Undefined".to_string(),
    IdlType::Boolean => "IdlType::Boolean".to_string(),
    IdlType::Numeric(n) => {
      let v = match n {
        NumericType::Byte => "Byte",
        NumericType::Octet => "Octet",
        NumericType::Short => "Short",
        NumericType::UnsignedShort => "UnsignedShort",
        NumericType::Long => "Long",
        NumericType::UnsignedLong => "UnsignedLong",
        NumericType::LongLong => "LongLong",
        NumericType::UnsignedLongLong => "UnsignedLongLong",
        NumericType::Float => "Float",
        NumericType::UnrestrictedFloat => "UnrestrictedFloat",
        NumericType::Double => "Double",
        NumericType::UnrestrictedDouble => "UnrestrictedDouble",
      };
      format!("IdlType::Numeric(NumericType::{v})")
    }
    IdlType::BigInt => "IdlType::BigInt".to_string(),
    IdlType::String(s) => {
      let v = match s {
        StringType::DomString => "DomString",
        StringType::ByteString => "ByteString",
        StringType::UsvString => "UsvString",
      };
      format!("IdlType::String(StringType::{v})")
    }
    IdlType::Object => "IdlType::Object".to_string(),
    IdlType::Symbol => "IdlType::Symbol".to_string(),
    IdlType::Named(NamedType { name, kind }) => {
      let kind_expr = match kind {
        NamedTypeKind::Unresolved => "NamedTypeKind::Unresolved",
        NamedTypeKind::Interface => "NamedTypeKind::Interface",
        NamedTypeKind::Dictionary => "NamedTypeKind::Dictionary",
        NamedTypeKind::Enum => "NamedTypeKind::Enum",
        NamedTypeKind::Typedef => "NamedTypeKind::Typedef",
        NamedTypeKind::CallbackFunction => "NamedTypeKind::CallbackFunction",
        NamedTypeKind::CallbackInterface => "NamedTypeKind::CallbackInterface",
      };
      format!(
        "IdlType::Named(NamedType {{ name: {}.to_string(), kind: {kind_expr} }})",
        rust_string_literal(name)
      )
    }
    IdlType::Nullable(inner) => format!(
      "IdlType::Nullable(Box::new({}))",
      render_webidl_ir_type(inner)
    ),
    IdlType::Union(members) => format!(
      "IdlType::Union(vec![{}])",
      members
        .iter()
        .map(render_webidl_ir_type)
        .collect::<Vec<_>>()
        .join(", ")
    ),
    IdlType::Sequence(inner) => format!(
      "IdlType::Sequence(Box::new({}))",
      render_webidl_ir_type(inner)
    ),
    IdlType::FrozenArray(inner) => {
      format!(
        "IdlType::FrozenArray(Box::new({}))",
        render_webidl_ir_type(inner)
      )
    }
    IdlType::AsyncSequence(inner) => {
      format!(
        "IdlType::AsyncSequence(Box::new({}))",
        render_webidl_ir_type(inner)
      )
    }
    IdlType::Record(key, value) => format!(
      "IdlType::Record(Box::new({}), Box::new({}))",
      render_webidl_ir_type(key),
      render_webidl_ir_type(value)
    ),
    IdlType::Promise(inner) => format!(
      "IdlType::Promise(Box::new({}))",
      render_webidl_ir_type(inner)
    ),
    IdlType::Annotated { annotations, inner } => {
      let annotations_expr = annotations
        .iter()
        .map(|ann| match ann {
          TypeAnnotation::Clamp => "TypeAnnotation::Clamp".to_string(),
          TypeAnnotation::EnforceRange => "TypeAnnotation::EnforceRange".to_string(),
          TypeAnnotation::LegacyNullToEmptyString => {
            "TypeAnnotation::LegacyNullToEmptyString".to_string()
          }
          TypeAnnotation::LegacyTreatNonObjectAsNull => {
            "TypeAnnotation::LegacyTreatNonObjectAsNull".to_string()
          }
          TypeAnnotation::AllowShared => "TypeAnnotation::AllowShared".to_string(),
          TypeAnnotation::AllowResizable => "TypeAnnotation::AllowResizable".to_string(),
          TypeAnnotation::Other { name, rhs } => format!(
            "TypeAnnotation::Other {{ name: {}.to_string(), rhs: {} }}",
            rust_string_literal(name),
            rhs
              .as_ref()
              .map(|rhs| format!("Some({}.to_string())", rust_string_literal(rhs)))
              .unwrap_or_else(|| "None".to_string())
          ),
        })
        .collect::<Vec<_>>()
        .join(", ");
      format!(
        "IdlType::Annotated {{ annotations: vec![{annotations_expr}], inner: Box::new({inner_expr}) }}",
        inner_expr = render_webidl_ir_type(inner),
      )
    }
  }
}

fn build_overload_ir_operation_set(
  resolved: &ResolvedWebIdlWorld,
  type_ctx: &webidl::ir::TypeContext,
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
  type_ctx: &webidl::ir::TypeContext,
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
  ty: &webidl::ir::IdlType,
) -> crate::webidl::overload_ir::TypeCategoryFastPath {
  use crate::webidl::overload_ir::TypeCategoryFastPath;
  let flattened = ty.flattened_union_member_types();
  TypeCategoryFastPath {
    category: ty.category_for_distinguishability(),
    innermost_named_type: match ty.innermost_type() {
      webidl::ir::IdlType::Named(named) => Some(named.clone()),
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
  cat: webidl::ir::DistinguishabilityCategory,
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
      webidl::ir::IdlType::Named(webidl::ir::NamedType {
        kind: webidl::ir::NamedTypeKind::Dictionary,
        ..
      })
    )
  })
}

fn interface_ids_for_fast_path(
  fp: &crate::webidl::overload_ir::TypeCategoryFastPath,
) -> Vec<(String, u32)> {
  fn interface_id_from_name_u32(name: &str) -> u32 {
    // Must match runtime interface ID generation (`webidl::interface_id_from_name`).
    webidl::interface_id_from_name(name).0
  }

  let mut out: Vec<(String, u32)> = Vec::new();

  for t in &fp.flattened_union_member_types {
    let webidl::ir::IdlType::Named(named) = t.innermost_type() else {
      continue;
    };
    if named.kind != webidl::ir::NamedTypeKind::Interface {
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
      .filter(|n| n.kind == webidl::ir::NamedTypeKind::Interface)
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

fn write_attribute_getter_wrapper(
  out: &mut String,
  interface: &str,
  attr_name: &str,
  is_static: bool,
) {
  let fn_name = attr_getter_fn_name(interface, attr_name, is_static);

  let receiver_expr = if interface == "Window" || is_static {
    "None"
  } else {
    "Some(this)"
  };
  let this_ident = if receiver_expr == "None" {
    "_this"
  } else {
    "this"
  };

  out.push_str(&format!(
    "#[allow(dead_code)]\nfn {fn_name}<Host, R>(rt: &mut R, host: &mut Host, {this_ident}: R::JsValue, _args: &[R::JsValue]) -> Result<R::JsValue, R::Error>\nwhere\n  R: crate::js::webidl::WebIdlBindingsRuntime<Host>,\n  Host: WebHostBindings<R>,\n{{\n",
  ));
  if receiver_expr == "Some(this)" {
    out.push_str("  if !rt_is_object::<Host, R>(rt, this) {\n");
    out.push_str("    return Err(rt_throw_type_error::<Host, R>(rt, \"Illegal invocation\"));\n");
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
) -> Result<()> {
  let ty_ir =
    crate::webidl::type_resolution::parse_type_with_world(resolved, &render_idl_type(ty), &[])
      .with_context(|| format!("parse attribute type for {interface}.{attr_name}"))?;

  let receiver_expr = if interface == "Window" || is_static {
    "None"
  } else {
    "Some(this)"
  };
  let this_ident = if receiver_expr == "None" {
    "_this"
  } else {
    "this"
  };

  let fn_name = attr_setter_fn_name(interface, attr_name, is_static);
  out.push_str(&format!(
    "#[allow(dead_code)]\nfn {fn_name}<Host, R>(rt: &mut R, host: &mut Host, {this_ident}: RtJsValue<Host, R>, args: &[RtJsValue<Host, R>]) -> Result<RtJsValue<Host, R>, RtError<Host, R>>\nwhere\n  R: crate::js::webidl::WebIdlBindingsRuntime<Host> + WebIdlJsRuntime<JsValue = RtJsValue<Host, R>, PropertyKey = RtPropertyKey<Host, R>, Error = RtError<Host, R>>,\n  Host: WebHostBindings<R>,\n{{\n",
  ));
  if receiver_expr == "Some(this)" {
    out.push_str("  if !rt_is_object::<Host, R>(rt, this) {\n");
    out.push_str("    return Err(rt_throw_type_error::<Host, R>(rt, \"Illegal invocation\"));\n");
    out.push_str("  }\n");
  }

  out.push_str("  static SCHEMAS: OnceLock<Vec<ArgumentSchema>> = OnceLock::new();\n");
  out.push_str("  let schemas = SCHEMAS.get_or_init(|| {\n");
  out.push_str(&format!(
    "    vec![ArgumentSchema {{ name: \"value\", ty: {ty}, optional: false, variadic: false, default: None }}]\n",
    ty = render_webidl_ir_type(&ty_ir)
  ));
  out.push_str("  });\n");
  out.push_str("  let ctx = type_context();\n");
  out.push_str("  let converted = convert_arguments(rt, args, schemas, ctx)?;\n");
  out.push_str(
    "  let value = converted\n    .into_iter()\n    .next()\n    .unwrap_or(ConvertedValue::Undefined);\n",
  );
  out.push_str("  let converted = converted_value_to_binding_value::<Host, R>(rt, ctx, &schemas[0].ty, value)?;\n");
  out.push_str(&format!(
    "  host.set_attribute(rt, {receiver_expr}, {iface_lit}, {attr_lit}, converted)?;\n",
    receiver_expr = receiver_expr,
    iface_lit = rust_string_literal(interface),
    attr_lit = rust_string_literal(attr_name),
  ));
  out.push_str("  Ok(rt_js_undefined::<Host, R>(rt))\n");
  out.push_str("}\n\n");
  Ok(())
}

fn write_operation_wrapper(
  out: &mut String,
  resolved: &ResolvedWebIdlWorld,
  type_ctx: &webidl::ir::TypeContext,
  interface: &str,
  op_name: &str,
  _iterable: Option<&IterableInfo>,
  overloads: &[OperationSig],
  is_static: bool,
  is_global: bool,
  _config: &WebIdlBindingsCodegenConfig,
) -> Result<()> {
  // Build schemas for `resolve_overload` and `convert_arguments`.
  let mut overload_sigs = Vec::<String>::new();
  let mut arg_schemas = Vec::<String>::new();

  for (decl_index, sig) in overloads.iter().enumerate() {
    let mut ol_args = Vec::<String>::new();
    let mut params = Vec::<String>::new();

    for arg in &sig.arguments {
      let schema_ty = type_resolution::parse_type_with_world(
        resolved,
        &ast_idl_type_to_webidl_ir_src(&arg.type_),
        &arg.ext_attrs,
      )
      .with_context(|| {
        format!(
          "parse argument type for {interface}.{op_name} argument `{}`",
          arg.name
        )
      })?;

      let overload_ty = expand_typedefs_in_type(type_ctx, &schema_ty).with_context(|| {
        format!(
          "expand typedefs in overload type for {interface}.{op_name} argument `{}`",
          arg.name
        )
      })?;

      let optionality = if arg.variadic {
        "Optionality::Variadic"
      } else if arg.optional || arg.default.is_some() {
        "Optionality::Optional"
      } else {
        "Optionality::Required"
      };

      ol_args.push(format!(
        "        OverloadArg {{ ty: {ty}, optionality: {optionality}, default: None }},\n",
        ty = render_webidl_ir_type(&overload_ty),
        optionality = optionality
      ));

      let default_expr = arg
        .default
        .as_ref()
        .and_then(idl_literal_to_webidl_ir_default_value)
        .map(|dv| format!("Some({})", render_webidl_ir_default_value(&dv)))
        .unwrap_or_else(|| "None".to_string());

      params.push(format!(
        "        ArgumentSchema {{ name: {name}, ty: {ty}, optional: {optional}, variadic: {variadic}, default: {default} }},\n",
        name = rust_string_literal(&arg.name),
        ty = render_webidl_ir_type(&schema_ty),
        optional = if arg.optional { "true" } else { "false" },
        variadic = if arg.variadic { "true" } else { "false" },
        default = default_expr,
      ));
    }

    overload_sigs.push(format!(
      "      OverloadSig {{\n        args: vec![\n{args}        ],\n        decl_index: {decl_index},\n        distinguishing_arg_index_by_arg_count: None,\n      }},\n",
      args = ol_args.concat(),
      decl_index = decl_index
    ));

    arg_schemas.push(format!(
      "      vec![\n{params}      ],\n",
      params = params.concat()
    ));
  }

  let fn_name = op_wrapper_fn_name(interface, op_name);

  let receiver_expr = if is_global || is_static {
    "None"
  } else {
    "Some(this)"
  };
  let this_ident = if receiver_expr == "None" {
    "_this"
  } else {
    "this"
  };

  out.push_str(&format!(
    "#[allow(dead_code)]\nfn {fn_name}<Host, R>(rt: &mut R, host: &mut Host, {this_ident}: RtJsValue<Host, R>, args: &[RtJsValue<Host, R>]) -> Result<RtJsValue<Host, R>, RtError<Host, R>>\nwhere\n  R: crate::js::webidl::WebIdlBindingsRuntime<Host> + WebIdlJsRuntime<JsValue = RtJsValue<Host, R>, PropertyKey = RtPropertyKey<Host, R>, Error = RtError<Host, R>>,\n  Host: WebHostBindings<R>,\n{{\n",
  ));

  if receiver_expr == "Some(this)" {
    out.push_str("  if !rt_is_object::<Host, R>(rt, this) {\n");
    out.push_str("    return Err(rt_throw_type_error::<Host, R>(rt, \"Illegal invocation\"));\n");
    out.push_str("  }\n");
  }

  out.push_str("  static ARG_SCHEMAS: OnceLock<Vec<Vec<ArgumentSchema>>> = OnceLock::new();\n");
  out.push_str("  let arg_schemas = ARG_SCHEMAS.get_or_init(|| {\n    vec![\n");
  out.push_str(&arg_schemas.concat());
  out.push_str("    ]\n  });\n");

  out.push_str("  let overload_index: usize = {\n");
  if overloads.len() == 1 {
    out.push_str("    0\n");
  } else {
    out.push_str("    static OVERLOADS: OnceLock<Vec<OverloadSig>> = OnceLock::new();\n");
    out.push_str("    let overloads = OVERLOADS.get_or_init(|| {\n      vec![\n");
    out.push_str(&overload_sigs.concat());
    out.push_str("      ]\n    });\n");
    out.push_str("    resolve_overload(rt, overloads, args)?.overload_index\n");
  }
  out.push_str("  };\n");

  out.push_str("  let params = &arg_schemas[overload_index];\n");
  out.push_str("  let ctx = type_context();\n");
  out.push_str("  let converted_args = convert_arguments(rt, args, params, ctx)?;\n");
  out.push_str(
    "  let mut converted_binding_args: Vec<BindingValue<RtJsValue<Host, R>>> = Vec::with_capacity(converted_args.len());\n",
  );
  out.push_str("  for (schema, value) in params.iter().zip(converted_args.into_iter()) {\n");
  out.push_str("    converted_binding_args.push(converted_value_to_binding_value::<Host, R>(rt, ctx, &schema.ty, value)?);\n");
  out.push_str("  }\n");
  out.push_str(&format!(
    "  let result = host.call_operation(rt, {receiver_expr}, {iface_lit}, {op_lit}, overload_index, converted_binding_args)?;\n",
    receiver_expr = receiver_expr,
    iface_lit = rust_string_literal(interface),
    op_lit = rust_string_literal(op_name),
  ));
  out.push_str("  binding_value_to_js::<Host, R>(rt, result)\n");
  out.push_str("}\n\n");
  Ok(())
}

fn write_operation_wrapper_old(
  out: &mut String,
  resolved: &ResolvedWebIdlWorld,
  type_ctx: &webidl::ir::TypeContext,
  interface: &str,
  op_name: &str,
  overloads: &[OperationSig],
  is_static: bool,
  is_global: bool,
  config: &WebIdlBindingsCodegenConfig,
) -> Result<()> {
  let _ = config;
  let fn_name = op_wrapper_fn_name(interface, op_name);
  let this_ident = if is_global || is_static {
    "_this"
  } else {
    "this"
  };
  let args_ident = if overloads.len() == 1 && overloads[0].arguments.is_empty() {
    "_args"
  } else {
    "args"
  };
  out.push_str(&format!(
    "#[allow(dead_code)]\nfn {fn_name}<Host, R>(rt: &mut R, host: &mut Host, {this_ident}: R::JsValue, {args_ident}: &[R::JsValue]) -> Result<R::JsValue, R::Error>\nwhere\n  R: crate::js::webidl::WebIdlBindingsRuntime<Host>,\n  Host: WebHostBindings<R>,\n{{\n",
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

      if fast_path_matches_category(fp, webidl::ir::DistinguishabilityCategory::String) {
        if let Some(prev) = string_candidate.replace(overload_idx) {
          if prev != overload_idx {
            bail!(
              "ambiguous overload dispatch for {display_name}: multiple string overloads at distinguishing index {d} (argcount={})",
              group.argument_count
            );
          }
        }
      }

      if fast_path_matches_category(fp, webidl::ir::DistinguishabilityCategory::CallbackFunction) {
        callback_candidate = Some(overload_idx);
      }
      if fast_path_matches_category(fp, webidl::ir::DistinguishabilityCategory::AsyncSequence) {
        async_sequence_candidate = Some(overload_idx);
      }
      if fast_path_matches_category(fp, webidl::ir::DistinguishabilityCategory::SequenceLike) {
        sequence_candidate = Some(overload_idx);
      }

      if fast_path_matches_category(fp, webidl::ir::DistinguishabilityCategory::Object)
        || fast_path_matches_category(fp, webidl::ir::DistinguishabilityCategory::DictionaryLike)
      {
        object_like_candidate = Some(overload_idx);
      }

      if fast_path_matches_category(fp, webidl::ir::DistinguishabilityCategory::Boolean) {
        boolean_candidate = Some(overload_idx);
      }
      if fast_path_matches_category(fp, webidl::ir::DistinguishabilityCategory::Numeric) {
        numeric_candidate = Some(overload_idx);
      }
      if fast_path_matches_category(fp, webidl::ir::DistinguishabilityCategory::BigInt) {
        bigint_candidate = Some(overload_idx);
      }
      if fast_path_matches_category(fp, webidl::ir::DistinguishabilityCategory::Symbol) {
        symbol_candidate = Some(overload_idx);
      }

      if fast_path_matches_category(fp, webidl::ir::DistinguishabilityCategory::InterfaceLike) {
        interface_like_candidates.push((overload_idx, interface_ids_for_fast_path(fp)));
      }
    }

    let emit_call = |overload_idx: usize| -> String {
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
      let cond = "rt.is_object(v) && {\n        let async_iter = rt.symbol_async_iterator()?;\n        let iter = rt.symbol_iterator()?;\n        let mut m = rt.get_method(host, v, async_iter)?;\n        if m.is_none() {\n          m = rt.get_method(host, v, iter)?;\n        }\n        m.is_some()\n      }"
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
      let cond = "rt.is_object(v) && {\n        let iter = rt.symbol_iterator()?;\n        rt.get_method(host, v, iter)?.is_some()\n      }"
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
  if arguments.is_empty() {
    out.push_str("  let converted_args: Vec<BindingValue<R::JsValue>> = Vec::new();\n");
  } else {
    out.push_str("  let mut converted_args: Vec<BindingValue<R::JsValue>> = Vec::new();\n");
  }
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
        let dict_expr = format!(
          "BindingValue::Union {{ member_type: {member_lit}.to_string(), value: Box::new(js_to_dict_{dict}::<Host, R>(rt, host, {value})?) }}",
          member_lit = rust_string_literal(dict_name),
          dict = to_snake_ident(dict_name),
          value = value_ident,
        );
        return format!(
          "if rt.is_undefined({value}) {{ {dict_expr} }} else {{ {converted} }}",
          value = value_ident,
          dict_expr = dict_expr,
          converted = converted
        );
      }
    }
  }

  // If the argument is missing or `undefined`, use the default if present, otherwise `undefined`.
  let mut default_expr = arg
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

  // For optional union arguments with an explicit default, preserve the union selection in the
  // binding-layer value so the host can distinguish which union member was chosen.
  if arg.default.is_some() {
    if let IdlType::Union(members) = &arg.type_ {
      let member_type = arg.default.as_ref().and_then(|lit| match lit {
        IdlLiteral::String(_) => members.iter().find_map(|m| match m {
          IdlType::Builtin(
            BuiltinType::DOMString | BuiltinType::USVString | BuiltinType::ByteString,
          ) => Some(render_idl_type(m)),
          IdlType::Named(name) if resolved.enums.contains_key(name) => Some(name.clone()),
          _ => None,
        }),
        IdlLiteral::Boolean(_) => members.iter().find_map(|m| match m {
          IdlType::Builtin(BuiltinType::Boolean) => Some("boolean".to_string()),
          _ => None,
        }),
        IdlLiteral::Number(_) => members.iter().find_map(|m| match m {
          IdlType::Builtin(
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
            | BuiltinType::UnrestrictedDouble,
          ) => Some(render_idl_type(m)),
          _ => None,
        }),
        _ => None,
      });
      if let Some(member_type) = member_type {
        default_expr = format!(
          "BindingValue::Union {{ member_type: {member_lit}.to_string(), value: Box::new({default_expr}) }}",
          member_lit = rust_string_literal(&member_type),
          default_expr = default_expr
        );
      }
    }
  }

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
    IdlType::Annotated { inner, .. } => type_contains_callback(resolved, inner),
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
    let (radix, digits) =
      if let Some(hex) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
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
    IdlLiteral::Undefined => "rt_js_undefined::<Host, R>(rt)".to_string(),
    IdlLiteral::Null => "rt_js_null::<Host, R>(rt)".to_string(),
    IdlLiteral::Boolean(b) => format!("rt.js_bool({})", if *b { "true" } else { "false" }),
    IdlLiteral::Number(n) => {
      let v = parse_idl_number_literal(n).unwrap_or(0.0);
      format!("rt_js_number::<Host, R>(rt, {v:?})")
    }
    IdlLiteral::String(s) => format!("rt.js_string({})?", rust_string_literal(s)),
    IdlLiteral::EmptyObject | IdlLiteral::EmptyArray | IdlLiteral::Identifier(_) => {
      "rt_js_undefined::<Host, R>(rt)".to_string()
    }
  }
}

fn emit_constant_vmjs_value_expr(lit: &IdlLiteral) -> String {
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
    let (radix, digits) =
      if let Some(hex) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
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
    IdlLiteral::Undefined => "Value::Undefined".to_string(),
    IdlLiteral::Null => "Value::Null".to_string(),
    IdlLiteral::Boolean(b) => format!("Value::Bool({})", if *b { "true" } else { "false" }),
    IdlLiteral::Number(n) => {
      let v = parse_idl_number_literal(n).unwrap_or(0.0);
      format!("Value::Number({v:?})")
    }
    IdlLiteral::String(s) => format!(
      "Value::String(rt.alloc_string({})?)",
      rust_string_literal(s)
    ),
    IdlLiteral::EmptyObject | IdlLiteral::EmptyArray | IdlLiteral::Identifier(_) => {
      "Value::Undefined".to_string()
    }
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
    format!(
      "conversions::IntegerConversionAttrs {{ clamp: {clamp}, enforce_range: {enforce_range} }}"
    )
  }
}

fn emit_conversion_expr(
  resolved: &ResolvedWebIdlWorld,
  ty: &IdlType,
  ext_attrs: &[ExtendedAttribute],
  value_ident: &str,
) -> String {
  match ty {
    IdlType::Annotated {
      ext_attrs: ty_ext_attrs,
      inner,
    } => {
      let mut merged = ty_ext_attrs.clone();
      merged.extend_from_slice(ext_attrs);
      emit_conversion_expr(resolved, inner, &merged, value_ident)
    }
    IdlType::Builtin(b) => match b {
      BuiltinType::Undefined => "BindingValue::Undefined".to_string(),
      BuiltinType::Any => format!("BindingValue::Object({value_ident})"),
      BuiltinType::Boolean => format!("BindingValue::Bool(rt.to_boolean({value_ident})?)"),
      BuiltinType::DOMString | BuiltinType::USVString | BuiltinType::ByteString => {
        let legacy_null_to_empty =
          ext_attrs.iter().any(|a| a.name.as_str() == "LegacyNullToEmptyString");
        // Avoid nested mutable borrows of `rt` by splitting `ToString` + `js_string_to_rust_string`
        // into two distinct steps.
        if legacy_null_to_empty {
          format!(
            "if rt.is_null({value_ident}) || rt.is_undefined({value_ident}) {{ BindingValue::String(String::new()) }} else {{ let s = rt.to_string(host, {value_ident})?; BindingValue::String(rt.js_string_to_rust_string(s)?) }}",
            value_ident = value_ident
          )
        } else {
          format!(
            "{{ let s = rt.to_string(host, {value_ident})?; BindingValue::String(rt.js_string_to_rust_string(s)?) }}"
          )
        }
      }
      BuiltinType::Object => format!("BindingValue::Object({value_ident})"),
      BuiltinType::Byte => format!(
        "BindingValue::Number(conversions::to_byte(rt, host, {value_ident}, {})? as f64)",
        emit_integer_conversion_attrs(ext_attrs)
      ),
      BuiltinType::Octet => format!(
        "BindingValue::Number(conversions::to_octet(rt, host, {value_ident}, {})? as f64)",
        emit_integer_conversion_attrs(ext_attrs)
      ),
      BuiltinType::Short => format!(
        "BindingValue::Number(conversions::to_short(rt, host, {value_ident}, {})? as f64)",
        emit_integer_conversion_attrs(ext_attrs)
      ),
      BuiltinType::UnsignedShort => format!(
        "BindingValue::Number(conversions::to_unsigned_short(rt, host, {value_ident}, {})? as f64)",
        emit_integer_conversion_attrs(ext_attrs)
      ),
      BuiltinType::Long => format!(
        "BindingValue::Number(conversions::to_long(rt, host, {value_ident}, {})? as f64)",
        emit_integer_conversion_attrs(ext_attrs)
      ),
      BuiltinType::UnsignedLong => format!(
        "BindingValue::Number(conversions::to_unsigned_long(rt, host, {value_ident}, {})? as f64)",
        emit_integer_conversion_attrs(ext_attrs)
      ),
      BuiltinType::LongLong => format!(
        "BindingValue::Number(conversions::to_long_long(rt, host, {value_ident}, {})? as f64)",
        emit_integer_conversion_attrs(ext_attrs)
      ),
      BuiltinType::UnsignedLongLong => format!(
        "BindingValue::Number(conversions::to_unsigned_long_long(rt, host, {value_ident}, {})? as f64)",
        emit_integer_conversion_attrs(ext_attrs)
      ),
      BuiltinType::Float => {
        format!("BindingValue::Number(conversions::to_float(rt, host, {value_ident})? as f64)")
      }
      BuiltinType::UnrestrictedFloat => format!(
        "BindingValue::Number(conversions::to_unrestricted_float(rt, host, {value_ident})? as f64)"
      ),
      BuiltinType::Double => format!("BindingValue::Number(conversions::to_double(rt, host, {value_ident})?)"),
      BuiltinType::UnrestrictedDouble => {
        format!("BindingValue::Number(conversions::to_unrestricted_double(rt, host, {value_ident})?)")
      }
    },
    IdlType::Named(name) => {
      if resolved.dictionaries.contains_key(name) {
        format!(
          "js_to_dict_{}::<Host, R>(rt, host, {value_ident})?",
          to_snake_ident(name)
        )
      } else if let Some(en) = resolved.enums.get(name) {
        let allowed = en
          .values
          .iter()
          .map(|v| rust_string_literal(v))
          .collect::<Vec<_>>()
          .join(", ");
        format!(
          "BindingValue::String(conversions::to_enum::<Host, R>(rt, host, {value_ident}, {enum_name}, &[{allowed}])?)",
          value_ident = value_ident,
          enum_name = rust_string_literal(name),
          allowed = allowed,
        )
      } else if resolved.typedefs.contains_key(name) {
        match resolved.resolve_typedef(name) {
          Ok(expanded) => emit_conversion_expr(resolved, &expanded, ext_attrs, value_ident),
          Err(_) => format!("BindingValue::Object({value_ident})"),
        }
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
    IdlType::Union(members) => emit_union_conversion_expr(resolved, members, value_ident),
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
    IdlType::Promise(_) => format!("BindingValue::Object({value_ident})"),
    IdlType::Record { key: _key, value } => {
      let value_expr = emit_conversion_expr(resolved, value, &[], "v");
      format!(
        "conversions::to_record(rt, host, {value_ident}, |rt, host, v| Ok({value_expr}))?",
        value_ident = value_ident,
        value_expr = value_expr,
      )
    }
  }
}

fn emit_union_conversion_expr(
  resolved: &ResolvedWebIdlWorld,
  members: &[IdlType],
  value_ident: &str,
) -> String {
  let mut has_undefined = false;
  let mut has_nullable = false;
  let mut has_any = false;
  let mut has_object = false;

  let mut sequence_member: Option<&IdlType> = None;
  let mut dict_member: Option<&String> = None;
  let mut record_member: Option<&IdlType> = None;
  let mut callback_function_member: Option<&String> = None;
  let mut callback_interface_member: Option<&String> = None;
  let mut interface_like: Vec<&String> = Vec::new();
  let mut boolean_member: Option<&IdlType> = None;
  let mut numeric_member: Option<&IdlType> = None;
  let mut string_member: Option<&IdlType> = None;

  for member in members {
    // Unwrap leading annotations for discrimination; keep `member` intact so conversion sees the
    // full type (including any annotations).
    let mut kind: &IdlType = member;
    while let IdlType::Annotated { inner, .. } = kind {
      kind = inner;
    }

    if let IdlType::Nullable(t) = kind {
      has_nullable = true;
      kind = t;
      while let IdlType::Annotated { inner, .. } = kind {
        kind = inner;
      }
    }

    match kind {
      IdlType::Builtin(BuiltinType::Undefined) => has_undefined = true,
      IdlType::Builtin(BuiltinType::Any) => has_any = true,
      IdlType::Builtin(BuiltinType::Object) => has_object = true,
      IdlType::Builtin(BuiltinType::Boolean) => {
        let _ = boolean_member.get_or_insert(member);
      }
      IdlType::Builtin(
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
        | BuiltinType::UnrestrictedDouble,
      ) => {
        let _ = numeric_member.get_or_insert(member);
      }
      IdlType::Builtin(
        BuiltinType::DOMString | BuiltinType::USVString | BuiltinType::ByteString,
      ) => {
        let _ = string_member.get_or_insert(member);
      }
      IdlType::Named(name) => {
        if resolved.callbacks.contains_key(name) {
          let _ = callback_function_member.get_or_insert(name);
        } else if resolved.interfaces.get(name).is_some_and(|i| i.callback) {
          let _ = callback_interface_member.get_or_insert(name);
        } else if resolved.dictionaries.contains_key(name) {
          let _ = dict_member.get_or_insert(name);
        } else if resolved.enums.contains_key(name) {
          // Enum conversion uses ToString + validation; treat it as a string-like member.
          let _ = string_member.get_or_insert(member);
        } else if resolved.interfaces.contains_key(name) {
          interface_like.push(name);
        }
      }
      IdlType::Sequence(_) | IdlType::FrozenArray(_) => {
        let _ = sequence_member.get_or_insert(member);
      }
      IdlType::Record { .. } => {
        let _ = record_member.get_or_insert(member);
      }
      IdlType::Union(_)
      | IdlType::Promise(_)
      | IdlType::Nullable(_)
      | IdlType::Annotated { .. } => {}
    }
  }

  let wrap = |member_type: &str, expr: String| -> String {
    format!(
      "BindingValue::Union {{ member_type: {member_type_lit}.to_string(), value: Box::new({expr}) }}",
      member_type_lit = rust_string_literal(member_type),
      expr = expr
    )
  };

  let dict_expr = dict_member.map(|dict| {
    wrap(
      dict,
      format!(
        "js_to_dict_{}::<Host, R>(rt, host, v)?",
        to_snake_ident(dict)
      ),
    )
  });
  let seq_expr = sequence_member.map(|ty| {
    wrap(
      &render_idl_type(ty),
      emit_conversion_expr(resolved, ty, &[], "v"),
    )
  });
  let record_expr = record_member.map(|ty| {
    wrap(
      &render_idl_type(ty),
      emit_conversion_expr(resolved, ty, &[], "v"),
    )
  });
  let callback_expr = callback_function_member.map(|name| {
    wrap(
      name,
      emit_conversion_expr(resolved, &IdlType::Named(name.clone()), &[], "v"),
    )
  });
  let callback_iface_expr = callback_interface_member.map(|name| {
    wrap(
      name,
      emit_conversion_expr(resolved, &IdlType::Named(name.clone()), &[], "v"),
    )
  });
  let boolean_expr = boolean_member.map(|ty| {
    wrap(
      &render_idl_type(ty),
      emit_conversion_expr(resolved, ty, &[], "v"),
    )
  });
  let numeric_expr = numeric_member.map(|ty| {
    wrap(
      &render_idl_type(ty),
      emit_conversion_expr(resolved, ty, &[], "v"),
    )
  });
  let string_expr = string_member.map(|ty| {
    wrap(
      &render_idl_type(ty),
      emit_conversion_expr(resolved, ty, &[], "v"),
    )
  });

  let any_expr = wrap("any", "BindingValue::Object(v)".to_string());
  let object_expr = wrap("object", "BindingValue::Object(v)".to_string());
  let null_expr = wrap("null", "BindingValue::Null".to_string());
  let undefined_expr = wrap("undefined", "BindingValue::Undefined".to_string());

  let mut out = String::new();
  out.push_str("{\n");
  out.push_str(&format!(
    "  let v = {value_ident};\n",
    value_ident = value_ident
  ));

  // Undefined member special-case.
  if has_undefined {
    out.push_str("  if rt.is_undefined(v) {\n    ");
    out.push_str(&undefined_expr);
    out.push_str("\n  }");
  } else {
    out.push_str("  if false {\n    BindingValue::Undefined\n  }");
  }

  // `null`/`undefined` dictionary special-case (dictionary converters treat them as "missing").
  if let Some(dict_expr) = &dict_expr {
    out.push_str(" else if rt.is_null(v) || rt.is_undefined(v) {\n    ");
    out.push_str(dict_expr);
    out.push_str("\n  }");
  }

  // Nullable special-case.
  if has_nullable {
    out.push_str(" else if rt.is_null(v) || rt.is_undefined(v) {\n    ");
    out.push_str(&null_expr);
    out.push_str("\n  }");
  }

  // Platform object / interface-like members.
  for iface in &interface_like {
    let iface_expr = wrap(iface, "BindingValue::Object(v)".to_string());
    out.push_str(&format!(
      " else if rt.is_platform_object(v) && rt.implements_interface(v, crate::js::webidl::interface_id_from_name({iface_lit})) {{\n    {iface_expr}\n  }}",
      iface_lit = rust_string_literal(iface),
      iface_expr = iface_expr
    ));
  }

  // Callback function members win over dictionary/record conversions for callable objects.
  if let Some(callback_expr) = &callback_expr {
    out.push_str(" else if rt.is_callable(v) {\n    ");
    out.push_str(callback_expr);
    out.push_str("\n  }");
  }

  // Object branch: sequence/record/dictionary/callback-interface/object.
  out.push_str(" else if rt.is_object(v) {\n");
  if let Some(seq_expr) = &seq_expr {
    out.push_str(
      "    let has_iter = {\n      let iterator_key = rt.symbol_iterator()?;\n      rt.get_method(host, v, iterator_key)?.is_some() || rt.is_array(v)?\n    };\n",
    );
    out.push_str("    if has_iter {\n      ");
    out.push_str(seq_expr);
    out.push_str("\n    }");

    // Dictionary/record should only be considered when the object is not iterable.
    if let Some(dict_expr) = &dict_expr {
      out.push_str(" else {\n      ");
      out.push_str(dict_expr);
      out.push_str("\n    }");
    } else if let Some(record_expr) = &record_expr {
      out.push_str(" else {\n      ");
      out.push_str(record_expr);
      out.push_str("\n    }");
    } else if let Some(callback_iface_expr) = &callback_iface_expr {
      out.push_str(" else {\n      ");
      out.push_str(callback_iface_expr);
      out.push_str("\n    }");
    } else if has_object || has_any {
      out.push_str(" else {\n      ");
      out.push_str(if has_object { &object_expr } else { &any_expr });
      out.push_str("\n    }");
    } else {
      out.push_str(" else {\n      return Err(rt.throw_type_error(\"Value is not a valid union type\"));\n    }");
    }
    out.push_str("\n  }");
  } else if let Some(dict_expr) = &dict_expr {
    out.push_str("    ");
    out.push_str(dict_expr);
    out.push_str("\n  }");
  } else if let Some(record_expr) = &record_expr {
    out.push_str("    ");
    out.push_str(record_expr);
    out.push_str("\n  }");
  } else if let Some(callback_iface_expr) = &callback_iface_expr {
    out.push_str("    ");
    out.push_str(callback_iface_expr);
    out.push_str("\n  }");
  } else if has_object || has_any {
    out.push_str("    ");
    out.push_str(if has_object { &object_expr } else { &any_expr });
    out.push_str("\n  }");
  } else {
    out.push_str("    return Err(rt.throw_type_error(\"Value is not a valid union type\"));\n  }");
  }

  // Primitive fast paths and fallthrough conversions.
  if let Some(boolean_expr) = &boolean_expr {
    out.push_str(" else if rt.is_boolean(v) {\n    ");
    out.push_str(boolean_expr);
    out.push_str("\n  }");
  }
  if let Some(numeric_expr) = &numeric_expr {
    out.push_str(" else if rt.is_number(v) {\n    ");
    out.push_str(numeric_expr);
    out.push_str("\n  }");
  }
  if let Some(string_expr) = &string_expr {
    out.push_str(" else if rt.is_string(v) || rt.is_string_object(v) {\n    ");
    out.push_str(string_expr);
    out.push_str("\n  }");
  }

  out.push_str(" else {\n    ");
  if let Some(string_expr) = &string_expr {
    out.push_str(string_expr);
    out.push_str("\n  }\n");
  } else if let Some(numeric_expr) = &numeric_expr {
    out.push_str(numeric_expr);
    out.push_str("\n  }\n");
  } else if let Some(boolean_expr) = &boolean_expr {
    out.push_str(boolean_expr);
    out.push_str("\n  }\n");
  } else if has_any {
    out.push_str(&any_expr);
    out.push_str("\n  }\n");
  } else {
    out.push_str("return Err(rt.throw_type_error(\"Value is not a valid union type\"));\n  }\n");
  }

  out.push_str("}\n");
  out
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
    IdlType::Annotated { inner, .. } => emit_type_predicate(resolved, inner, value_expr),
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
    IdlType::Sequence(_)
    | IdlType::FrozenArray(_)
    | IdlType::Promise(_)
    | IdlType::Record { .. } => "true".to_string(),
  }
}

fn write_constructor_wrapper(
  out: &mut String,
  resolved: &ResolvedWebIdlWorld,
  type_ctx: &webidl::ir::TypeContext,
  interface: &str,
  overloads: &[ArgumentList],
  _config: &WebIdlBindingsCodegenConfig,
) -> Result<()> {
  let mut overload_sigs = Vec::<String>::new();
  let mut arg_schemas = Vec::<String>::new();

  for (decl_index, sig) in overloads.iter().enumerate() {
    let mut ol_args = Vec::<String>::new();
    let mut params = Vec::<String>::new();

    for arg in &sig.arguments {
      let schema_ty = type_resolution::parse_type_with_world(
        resolved,
        &ast_idl_type_to_webidl_ir_src(&arg.type_),
        &arg.ext_attrs,
      )
      .with_context(|| {
        format!(
          "parse constructor argument type for {interface} argument `{}`",
          arg.name
        )
      })?;

      let overload_ty = expand_typedefs_in_type(type_ctx, &schema_ty).with_context(|| {
        format!(
          "expand typedefs in constructor overload type for {interface} argument `{}`",
          arg.name
        )
      })?;

      let optionality = if arg.variadic {
        "Optionality::Variadic"
      } else if arg.optional || arg.default.is_some() {
        "Optionality::Optional"
      } else {
        "Optionality::Required"
      };

      ol_args.push(format!(
        "        OverloadArg {{ ty: {ty}, optionality: {optionality}, default: None }},\n",
        ty = render_webidl_ir_type(&overload_ty),
        optionality = optionality
      ));

      let default_expr = arg
        .default
        .as_ref()
        .and_then(idl_literal_to_webidl_ir_default_value)
        .map(|dv| format!("Some({})", render_webidl_ir_default_value(&dv)))
        .unwrap_or_else(|| "None".to_string());

      params.push(format!(
        "        ArgumentSchema {{ name: {name}, ty: {ty}, optional: {optional}, variadic: {variadic}, default: {default} }},\n",
        name = rust_string_literal(&arg.name),
        ty = render_webidl_ir_type(&schema_ty),
        optional = if arg.optional { "true" } else { "false" },
        variadic = if arg.variadic { "true" } else { "false" },
        default = default_expr,
      ));
    }

    overload_sigs.push(format!(
      "      OverloadSig {{\n        args: vec![\n{args}        ],\n        decl_index: {decl_index},\n        distinguishing_arg_index_by_arg_count: None,\n      }},\n",
      args = ol_args.concat(),
      decl_index = decl_index
    ));

    arg_schemas.push(format!(
      "      vec![\n{params}      ],\n",
      params = params.concat()
    ));
  }

  let fn_name = ctor_wrapper_fn_name(interface);
  out.push_str(&format!(
    "#[allow(dead_code)]\nfn {fn_name}<Host, R>(rt: &mut R, host: &mut Host, _this: RtJsValue<Host, R>, args: &[RtJsValue<Host, R>]) -> Result<RtJsValue<Host, R>, RtError<Host, R>>\nwhere\n  R: crate::js::webidl::WebIdlBindingsRuntime<Host> + WebIdlJsRuntime<JsValue = RtJsValue<Host, R>, PropertyKey = RtPropertyKey<Host, R>, Error = RtError<Host, R>>,\n  Host: WebHostBindings<R>,\n{{\n",
  ));

  out.push_str("  static ARG_SCHEMAS: OnceLock<Vec<Vec<ArgumentSchema>>> = OnceLock::new();\n");
  out.push_str("  let arg_schemas = ARG_SCHEMAS.get_or_init(|| {\n    vec![\n");
  out.push_str(&arg_schemas.concat());
  out.push_str("    ]\n  });\n");

  out.push_str("  let overload_index: usize = {\n");
  if overloads.len() == 1 {
    out.push_str("    0\n");
  } else {
    out.push_str("    static OVERLOADS: OnceLock<Vec<OverloadSig>> = OnceLock::new();\n");
    out.push_str("    let overloads = OVERLOADS.get_or_init(|| {\n      vec![\n");
    out.push_str(&overload_sigs.concat());
    out.push_str("      ]\n    });\n");
    out.push_str("    resolve_overload(rt, overloads, args)?.overload_index\n");
  }
  out.push_str("  };\n");

  out.push_str("  let params = &arg_schemas[overload_index];\n");
  out.push_str("  let ctx = type_context();\n");
  out.push_str("  let converted_args = convert_arguments(rt, args, params, ctx)?;\n");
  out.push_str(
    "  let mut converted_binding_args: Vec<BindingValue<RtJsValue<Host, R>>> = Vec::with_capacity(converted_args.len());\n",
  );
  out.push_str("  for (schema, value) in params.iter().zip(converted_args.into_iter()) {\n");
  out.push_str("    converted_binding_args.push(converted_value_to_binding_value::<Host, R>(rt, ctx, &schema.ty, value)?);\n");
  out.push_str("  }\n");
  out.push_str(&format!(
    "  let result = host.call_operation(rt, None, {iface_lit}, \"constructor\", overload_index, converted_binding_args)?;\n",
    iface_lit = rust_string_literal(interface)
  ));
  out.push_str("  binding_value_to_js::<Host, R>(rt, result)\n");
  out.push_str("}\n\n");
  Ok(())
}

fn write_constructor_wrapper_old(
  out: &mut String,
  resolved: &ResolvedWebIdlWorld,
  type_ctx: &webidl::ir::TypeContext,
  interface: &str,
  overloads: &[ArgumentList],
  _config: &WebIdlBindingsCodegenConfig,
) -> Result<()> {
  let fn_name = ctor_wrapper_fn_name(interface);
  let args_ident = if overloads.len() == 1 && overloads[0].arguments.is_empty() {
    "_args"
  } else {
    "args"
  };
  out.push_str(&format!(
    "#[allow(dead_code)]\nfn {fn_name}<Host, R>(rt: &mut R, host: &mut Host, this: R::JsValue, {args_ident}: &[R::JsValue]) -> Result<R::JsValue, R::Error>\nwhere\n  R: crate::js::webidl::WebIdlBindingsRuntime<Host>,\n  Host: WebHostBindings<R>,\n{{\n",
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

      if fast_path_matches_category(fp, webidl::ir::DistinguishabilityCategory::String) {
        if let Some(prev) = string_candidate.replace(overload_idx) {
          if prev != overload_idx {
            bail!(
              "ambiguous overload dispatch for {display_name}: multiple string overloads at distinguishing index {d} (argcount={})",
              group.argument_count
            );
          }
        }
      }

      if fast_path_matches_category(fp, webidl::ir::DistinguishabilityCategory::CallbackFunction) {
        callback_candidate = Some(overload_idx);
      }
      if fast_path_matches_category(fp, webidl::ir::DistinguishabilityCategory::AsyncSequence) {
        async_sequence_candidate = Some(overload_idx);
      }
      if fast_path_matches_category(fp, webidl::ir::DistinguishabilityCategory::SequenceLike) {
        sequence_candidate = Some(overload_idx);
      }

      if fast_path_matches_category(fp, webidl::ir::DistinguishabilityCategory::Object)
        || fast_path_matches_category(fp, webidl::ir::DistinguishabilityCategory::DictionaryLike)
      {
        object_like_candidate = Some(overload_idx);
      }

      if fast_path_matches_category(fp, webidl::ir::DistinguishabilityCategory::Boolean) {
        boolean_candidate = Some(overload_idx);
      }
      if fast_path_matches_category(fp, webidl::ir::DistinguishabilityCategory::Numeric) {
        numeric_candidate = Some(overload_idx);
      }
      if fast_path_matches_category(fp, webidl::ir::DistinguishabilityCategory::BigInt) {
        bigint_candidate = Some(overload_idx);
      }
      if fast_path_matches_category(fp, webidl::ir::DistinguishabilityCategory::Symbol) {
        symbol_candidate = Some(overload_idx);
      }

      if fast_path_matches_category(fp, webidl::ir::DistinguishabilityCategory::InterfaceLike) {
        interface_like_candidates.push((overload_idx, interface_ids_for_fast_path(fp)));
      }
    }

    let emit_call = |overload_idx: usize| -> String {
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
      let cond = "rt.is_object(v) && {\n        let async_iter = rt.symbol_async_iterator()?;\n        let iter = rt.symbol_iterator()?;\n        let mut m = rt.get_method(host, v, async_iter)?;\n        if m.is_none() {\n          m = rt.get_method(host, v, iter)?;\n        }\n        m.is_some()\n      }"
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
      let cond = "rt.is_object(v) && {\n        let iter = rt.symbol_iterator()?;\n        rt.get_method(host, v, iter)?.is_some()\n      }"
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
  if arguments.is_empty() {
    out.push_str("  let converted_args: Vec<BindingValue<R::JsValue>> = Vec::new();\n");
  } else {
    out.push_str("  let mut converted_args: Vec<BindingValue<R::JsValue>> = Vec::new();\n");
  }
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
    "  let _ = host.call_operation(rt, Some(this), {iface_lit}, \"constructor\", {overload_idx}, converted_args)?;\n",
    iface_lit = rust_string_literal(interface),
    overload_idx = overload_idx
  ));
  out.push_str("  Ok(rt.js_undefined())\n");
  out.push_str("}\n");
  out
}

fn write_operation_wrapper_vmjs(
  out: &mut String,
  resolved: &ResolvedWebIdlWorld,
  interface: &str,
  op_name: &str,
  iterable: Option<&IterableInfo>,
  overloads: &[OperationSig],
  is_static: bool,
  is_global: bool,
) {
  let fn_name = op_wrapper_fn_name(interface, op_name);
  let host_ident = if overloads
    .iter()
    .any(|sig| args_need_host_vmjs(resolved, &sig.arguments))
  {
    "host"
  } else {
    "_host"
  };
  let this_ident = if is_global || is_static {
    "_this"
  } else {
    "this"
  };
  let args_ident = if overloads.len() == 1 && overloads[0].arguments.is_empty() {
    "_args"
  } else {
    "args"
  };
  out.push_str(&format!(
    "#[allow(dead_code)]\nfn {fn_name}(\n  vm: &mut Vm,\n  scope: &mut Scope<'_>,\n  {host_ident}: &mut dyn VmHost,\n  hooks: &mut dyn VmHostHooks,\n  _callee: GcObject,\n  {this_ident}: Value,\n  {args_ident}: &[Value],\n) -> Result<Value, VmError>\n{{\n",
  ));
  out.push_str("  let mut rt = BindingsRuntime::from_scope(vm, scope.reborrow());\n");
  out.push_str("  let rt = &mut rt;\n");

  let receiver_expr = if is_global || is_static {
    "None"
  } else {
    "Some(this)"
  };
  if !(is_global || is_static) {
    out.push_str("  rt.scope.push_root(this)?;\n");
  }
  out.push_str(&format!("  let receiver = {receiver_expr};\n"));

  if iterable.is_some() && !is_static && !is_global {
    match op_name {
      "entries" | "keys" | "values" => {
        let kind = match op_name {
          "entries" => "IterableKind::Entries",
          "keys" => "IterableKind::Keys",
          "values" => "IterableKind::Values",
          _ => unreachable!(),
        };
        out.push_str(&format!("  let _ = {args_ident};\n"));
        out.push_str("  let bindings_host = host_from_hooks(hooks)?;\n");
        out.push_str(&format!(
          "  let snapshot = bindings_host.iterable_snapshot(&mut *rt.vm, &mut rt.scope, receiver, {iface_lit}, {kind})?;\n",
          iface_lit = rust_string_literal(interface),
          kind = kind,
        ));
        out.push_str("  let arr = rt.alloc_array(snapshot.len())?;\n");
        out.push_str("  for (idx, item) in snapshot.into_iter().enumerate() {\n");
        out.push_str("    let value = rt.binding_value_to_js(item)?;\n");
        out.push_str("    let value = rt.scope.push_root(value)?;\n");
        out.push_str("    let key_s = rt.scope.alloc_string(&idx.to_string())?;\n");
        out.push_str("    rt.scope.push_root(Value::String(key_s))?;\n");
        out.push_str("    let key = vm_js::PropertyKey::from_string(key_s);\n");
        out.push_str("    rt.scope.create_data_property_or_throw(arr, key, value)?;\n");
        out.push_str("  }\n");
        out.push_str("  let intr = rt\n");
        out.push_str("    .vm\n");
        out.push_str("    .intrinsics()\n");
        out.push_str("    .ok_or(VmError::Unimplemented(\"intrinsics not initialized\"))?;\n");
        out.push_str(
          "  let iterator_key = vm_js::PropertyKey::from_symbol(intr.well_known_symbols().iterator);\n",
        );
        out.push_str("  let Some(method) = rt.vm.get_method_from_object(&mut rt.scope, arr, iterator_key)? else {\n");
        out.push_str(
          "    return Err(rt.throw_type_error(\"iterable snapshot array is not iterable\"));\n",
        );
        out.push_str("  };\n");
        out.push_str(&format!(
          "  rt.vm.call_with_host_and_hooks({host_ident}, &mut rt.scope, hooks, method, Value::Object(arr), &[])\n"
        ));
        out.push_str("}\n\n");
        return;
      }
      "forEach" => {
        out.push_str(&format!(
          "  let callback = {args_ident}.get(0).copied().unwrap_or(Value::Undefined);\n  let callback = rt.scope.push_root(callback)?;\n  let this_arg = {args_ident}.get(1).copied().unwrap_or(Value::Undefined);\n  let this_arg = rt.scope.push_root(this_arg)?;\n",
        ));
        out.push_str("  let bindings_host = host_from_hooks(hooks)?;\n");
        out.push_str(&format!(
          "  let snapshot = bindings_host.iterable_snapshot(&mut *rt.vm, &mut rt.scope, receiver, {iface_lit}, IterableKind::Entries)?;\n",
          iface_lit = rust_string_literal(interface),
        ));
        out.push_str("  for entry in snapshot {\n");
        out.push_str("    let BindingValue::Sequence(pair) = entry else {\n");
        out.push_str(
          "      return Err(rt.throw_type_error(\"iterable forEach: expected [key, value] pair\"));\n",
        );
        out.push_str("    };\n");
        out.push_str("    if pair.len() != 2 {\n");
        out.push_str(
          "      return Err(rt.throw_type_error(\"iterable forEach: expected [key, value] pair\"));\n",
        );
        out.push_str("    }\n");
        out.push_str("    let mut iter = pair.into_iter();\n");
        out.push_str("    let key = iter.next().ok_or_else(|| rt.throw_type_error(\"iterable forEach: expected [key, value] pair\"))?;\n");
        out.push_str("    let value = iter.next().ok_or_else(|| rt.throw_type_error(\"iterable forEach: expected [key, value] pair\"))?;\n");
        out.push_str("    let key_js = rt.binding_value_to_js(key)?;\n");
        out.push_str("    let key_js = rt.scope.push_root(key_js)?;\n");
        out.push_str("    let value_js = rt.binding_value_to_js(value)?;\n");
        out.push_str("    let value_js = rt.scope.push_root(value_js)?;\n");
        out.push_str(&format!(
          "    let _ = rt.vm.call_with_host_and_hooks({host_ident}, &mut rt.scope, hooks, callback, this_arg, &[value_js, key_js, this])?;\n"
        ));
        out.push_str("  }\n");
        out.push_str("  Ok(Value::Undefined)\n");
        out.push_str("}\n\n");
        return;
      }
      _ => {}
    }
  }

  if overloads.len() == 1 {
    out.push_str(&emit_overload_call_vmjs(
      resolved,
      interface,
      op_name,
      "receiver",
      0,
      &overloads[0].arguments,
    ));
    out.push_str("}\n\n");
    return;
  }

  // WebIDL overload resolution clamps `arguments.length` to the maximum number of arguments in the
  // overload set (extra arguments are ignored unless an overload is variadic). Without this, calls
  // like `alert(\"a\", \"b\")` incorrectly fail to match `alert(DOMString)`.
  //
  // Note: if any overload is variadic (`max_arg_count == None`), the maximum argument count is
  // unbounded, so we must not truncate the argument slice.
  let max_argc = overloads
    .iter()
    .map(|sig| max_arg_count(&sig.arguments))
    .collect::<Vec<_>>();
  if max_argc.iter().all(|v| v.is_some()) {
    let max_argc = max_argc.into_iter().flatten().max().unwrap_or(0);
    out.push_str(&format!(
      "  let args = if args.len() > {max_argc} {{ &args[..{max_argc}] }} else {{ args }};\n",
      max_argc = max_argc
    ));
  }

  // If each overload's (required..=max) argument-count range is disjoint, argument count alone is
  // enough to select the overload, so emitting a first-argument type predicate is unnecessary (and
  // can incorrectly reject valid inputs that are convertible via WebIDL, e.g. `DOMString`).
  let ranges = overloads
    .iter()
    .map(|sig| {
      (
        required_arg_count(&sig.arguments),
        max_arg_count(&sig.arguments),
      )
    })
    .collect::<Vec<_>>();
  let ranges_overlap = |a_min: usize, a_max: Option<usize>, b_min: usize, b_max: Option<usize>| {
    let a_max = a_max.unwrap_or(usize::MAX);
    let b_max = b_max.unwrap_or(usize::MAX);
    a_min <= b_max && b_min <= a_max
  };
  let use_type_predicate = (0..ranges.len())
    .map(|idx| {
      let (min, max) = ranges[idx];
      (0..ranges.len()).any(|j| {
        if j == idx {
          return false;
        }
        let (other_min, other_max) = ranges[j];
        ranges_overlap(min, max, other_min, other_max)
      })
    })
    .collect::<Vec<_>>();

  for (idx, sig) in overloads.iter().enumerate() {
    let cond = emit_overload_condition_vmjs(sig, "args", use_type_predicate[idx]);
    if idx == 0 {
      out.push_str(&format!("  if {cond} {{\n"));
    } else {
      out.push_str(&format!("  }} else if {cond} {{\n"));
    }
    out.push_str(&indent_lines(
      &emit_overload_call_vmjs(
        resolved,
        interface,
        op_name,
        "receiver",
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

fn write_attribute_getter_wrapper_vmjs(
  out: &mut String,
  interface: &str,
  attr: &AttributeSig,
  is_static: bool,
  is_global: bool,
) {
  let fn_name = attr_getter_fn_name(interface, &attr.name, is_static);
  out.push_str(&format!(
    "#[allow(dead_code)]\nfn {fn_name}(\n  vm: &mut Vm,\n  scope: &mut Scope<'_>,\n  _host: &mut dyn VmHost,\n  hooks: &mut dyn VmHostHooks,\n  _callee: GcObject,\n  this: Value,\n  _args: &[Value],\n) -> Result<Value, VmError>\n{{\n",
  ));
  out.push_str("  let mut rt = BindingsRuntime::from_scope(vm, scope.reborrow());\n");
  out.push_str("  let rt = &mut rt;\n");

  let receiver_expr = if is_global || is_static {
    "None"
  } else {
    "Some(this)"
  };
  if !(is_global || is_static) {
    out.push_str("  rt.scope.push_root(this)?;\n");
  }
  out.push_str(&format!("  let receiver = {receiver_expr};\n"));

  out.push_str("  let bindings_host = host_from_hooks(hooks)?;\n");
  out.push_str(&format!(
    "  bindings_host.call_operation(&mut *rt.vm, &mut rt.scope, receiver, {iface_lit}, {attr_lit}, 0, &[])\n",
    iface_lit = rust_string_literal(interface),
    attr_lit = rust_string_literal(&attr.name),
  ));
  out.push_str("}\n\n");
}

fn write_attribute_setter_wrapper_vmjs(
  out: &mut String,
  resolved: &ResolvedWebIdlWorld,
  interface: &str,
  attr: &AttributeSig,
  is_static: bool,
  is_global: bool,
) {
  let fn_name = attr_setter_fn_name(interface, &attr.name, is_static);
  out.push_str(&format!(
    "#[allow(dead_code)]\nfn {fn_name}(\n  vm: &mut Vm,\n  scope: &mut Scope<'_>,\n  host: &mut dyn VmHost,\n  hooks: &mut dyn VmHostHooks,\n  _callee: GcObject,\n  this: Value,\n  args: &[Value],\n) -> Result<Value, VmError>\n{{\n",
  ));
  out.push_str("  let mut rt = BindingsRuntime::from_scope(vm, scope.reborrow());\n");
  out.push_str("  let rt = &mut rt;\n");

  let receiver_expr = if is_global || is_static {
    "None"
  } else {
    "Some(this)"
  };
  if !(is_global || is_static) {
    out.push_str("  rt.scope.push_root(this)?;\n");
  }
  out.push_str(&format!("  let receiver = {receiver_expr};\n"));

  out.push_str("  {\n    let mut converted_args: Vec<Value> = Vec::new();\n");
  out.push_str("    let v0 = if args.len() > 0 { args[0] } else { Value::Undefined };\n");
  out.push_str(&format!(
    "    let converted = {};\n",
    emit_conversion_expr_vmjs(resolved, &attr.type_, "v0", true),
  ));
  out.push_str("    let converted = rt.scope.push_root(converted)?;\n");
  out.push_str("    converted_args.push(converted);\n");
  out.push_str("    let bindings_host = host_from_hooks(hooks)?;\n");
  out.push_str(&format!(
    "    let _ = bindings_host.call_operation(&mut *rt.vm, &mut rt.scope, receiver, {iface_lit}, {attr_lit}, 0, &converted_args)?;\n",
    iface_lit = rust_string_literal(interface),
    attr_lit = rust_string_literal(&attr.name),
  ));
  out.push_str("    Ok(Value::Undefined)\n  }\n");
  out.push_str("}\n\n");
}

fn emit_overload_condition_vmjs(
  sig: &OperationSig,
  args_ident: &str,
  use_type_predicate: bool,
) -> String {
  let required = required_arg_count(&sig.arguments);
  let max = max_arg_count(&sig.arguments);
  let len_check = emit_args_len_check(args_ident, required, max);

  // If there are multiple overloads, we use the first argument's predicate as a best-effort
  // discriminator (works for the MVP overload shapes we care about).
  if sig.arguments.is_empty() || !use_type_predicate {
    return len_check;
  }

  let pred = emit_type_predicate_vmjs(&sig.arguments[0].type_, &format!("{args_ident}[0]"));
  if required == 0 {
    format!("{len_check} && ({args_ident}.len() == 0 || ({pred}))")
  } else {
    format!("{len_check} && ({pred})")
  }
}

fn emit_args_len_check(args_ident: &str, required: usize, max: Option<usize>) -> String {
  match max {
    Some(max) => {
      if required == max {
        // Prefer `len() == N` when the overload has a fixed arity.
        // This avoids `len() >= 0`-style always-true comparisons and keeps generated code easier to
        // read.
        format!("{args_ident}.len() == {required}")
      } else if required == 0 {
        // `len() >= 0` is always true for `usize` and triggers `unused_comparisons`.
        format!("{args_ident}.len() <= {max}")
      } else {
        format!("{args_ident}.len() >= {required} && {args_ident}.len() <= {max}")
      }
    }
    None => {
      if required == 0 {
        // Avoid generating `len() >= 0` (always true).
        "true".to_string()
      } else {
        format!("{args_ident}.len() >= {required}")
      }
    }
  }
}

fn emit_overload_call_vmjs(
  resolved: &ResolvedWebIdlWorld,
  interface: &str,
  operation: &str,
  receiver_expr: &str,
  overload_idx: usize,
  arguments: &[Argument],
) -> String {
  let mut out = String::new();
  out.push_str("  {\n");
  if arguments.is_empty() {
    out.push_str("    let converted_args: Vec<Value> = Vec::new();\n");
  } else {
    out.push_str("    let mut converted_args: Vec<Value> = Vec::new();\n");
  }

  let timer_policy_op = interface == "Window" && matches!(operation, "setTimeout" | "setInterval");

  for (idx, arg) in arguments.iter().enumerate() {
    if arg.variadic {
      out.push_str(&format!(
        "    for v in args.iter().copied().skip({idx}) {{\n      let converted = {};\n      let converted = rt.scope.push_root(converted)?;\n      converted_args.push(converted);\n    }}\n",
        emit_conversion_expr_vmjs(resolved, &arg.type_, "v", true)
      ));
      break;
    }

    out.push_str(&format!(
      "    let v{idx} = if args.len() > {idx} {{ args[{idx}] }} else {{ Value::Undefined }};\n",
      idx = idx
    ));

    // Project policy: HTML's `TimerHandler` (`setTimeout` / `setInterval`) accepts string handlers,
    // but FastRender intentionally rejects them to avoid eval-like behaviour. The host dispatch also
    // enforces this, but the vm-js bindings should reject before calling into the host.
    //
    // Additionally, we intentionally *avoid* WebIDL union conversion for `TimerHandler`, since that
    // would convert non-callable primitives (like numbers) into strings, producing misleading
    // "string handler" errors instead of the more appropriate "not callable".
    if timer_policy_op && idx == 0 && arg.name == "handler" {
      let (string_err, not_callable_err) = match operation {
        "setTimeout" => (
          "setTimeout does not currently support string handlers",
          "setTimeout callback is not callable",
        ),
        "setInterval" => (
          "setInterval does not currently support string handlers",
          "setInterval callback is not callable",
        ),
        _ => unreachable!(),
      };
      out.push_str(&format!(
        "    if matches!(v{idx}, Value::String(_)) {{\n      return Err(rt.throw_type_error({string_err}));\n    }}\n",
        idx = idx,
        string_err = rust_string_literal(string_err),
      ));
      out.push_str(&format!(
        "    if !rt.scope.heap().is_callable(v{idx})? {{\n      return Err(rt.throw_type_error({not_callable_err}));\n    }}\n",
        idx = idx,
        not_callable_err = rust_string_literal(not_callable_err),
      ));
      out.push_str(&format!("    let converted = v{idx};\n", idx = idx));
      out.push_str("    let converted = rt.scope.push_root(converted)?;\n");
      out.push_str("    converted_args.push(converted);\n");
      continue;
    }

    let expr = emit_conversion_expr_for_optional_vmjs(resolved, arg, &format!("v{idx}"), true);
    out.push_str(&format!("    let converted = {expr};\n"));
    out.push_str("    let converted = rt.scope.push_root(converted)?;\n");
    out.push_str("    converted_args.push(converted);\n");
  }

  out.push_str("    let bindings_host = host_from_hooks(hooks)?;\n");
  out.push_str(&format!(
    "    bindings_host.call_operation(&mut *rt.vm, &mut rt.scope, {receiver_expr}, {iface_lit}, {op_lit}, {overload_idx}, &converted_args)\n",
    receiver_expr = receiver_expr,
    iface_lit = rust_string_literal(interface),
    op_lit = rust_string_literal(operation),
    overload_idx = overload_idx,
  ));
  out.push_str("  }\n");
  out
}

fn type_needs_host_vmjs(resolved: &ResolvedWebIdlWorld, ty: &IdlType) -> bool {
  match ty {
    IdlType::Annotated { inner, .. } => type_needs_host_vmjs(resolved, inner),
    IdlType::Builtin(b) => match b {
      BuiltinType::DOMString | BuiltinType::USVString | BuiltinType::ByteString => true,
      BuiltinType::Long
      | BuiltinType::UnsignedLong
      | BuiltinType::Byte
      | BuiltinType::Octet
      | BuiltinType::Short
      | BuiltinType::UnsignedShort
      | BuiltinType::LongLong
      | BuiltinType::UnsignedLongLong
      | BuiltinType::Float
      | BuiltinType::UnrestrictedFloat
      | BuiltinType::Double
      | BuiltinType::UnrestrictedDouble => true,
      BuiltinType::Undefined | BuiltinType::Any | BuiltinType::Object | BuiltinType::Boolean => {
        false
      }
    },
    IdlType::Named(name) => {
      if resolved.dictionaries.contains_key(name) {
        true
      } else if resolved.enums.contains_key(name) {
        true
      } else if name == "Function" {
        true
      } else if resolved.callbacks.contains_key(name) {
        true
      } else if resolved
        .interfaces
        .get(name)
        .is_some_and(|iface| iface.callback)
      {
        true
      } else if resolved.typedefs.contains_key(name) {
        resolved
          .resolve_typedef(name)
          .ok()
          .is_some_and(|ty| type_needs_host_vmjs(resolved, &ty))
      } else {
        false
      }
    }
    IdlType::Nullable(inner) => type_needs_host_vmjs(resolved, inner),
    IdlType::Union(members) => members.iter().any(|m| type_needs_host_vmjs(resolved, m)),
    IdlType::Sequence(_) | IdlType::FrozenArray(_) => true,
    IdlType::Promise(_) => false,
    IdlType::Record { .. } => true,
  }
}

fn args_need_host_vmjs(resolved: &ResolvedWebIdlWorld, args: &[Argument]) -> bool {
  args
    .iter()
    .any(|arg| type_needs_host_vmjs(resolved, &arg.type_))
}

fn write_constructor_wrapper_vmjs(
  out: &mut String,
  resolved: &ResolvedWebIdlWorld,
  interface: &str,
  overloads: &[ArgumentList],
) {
  // Call without `new`.
  let call_fn_name = ctor_call_without_new_fn_name(interface);
  out.push_str(&format!(
    "#[allow(dead_code)]\nfn {call_fn_name}(\n  vm: &mut Vm,\n  scope: &mut Scope<'_>,\n  _host: &mut dyn VmHost,\n  _hooks: &mut dyn VmHostHooks,\n  _callee: GcObject,\n  _this: Value,\n  _args: &[Value],\n) -> Result<Value, VmError>\n{{\n",
  ));
  out.push_str("  let mut rt = BindingsRuntime::from_scope(vm, scope.reborrow());\n");
  out.push_str("  let rt = &mut rt;\n");
  let call_without_new_message = if interface == "Document" {
    "Document constructor cannot be invoked without 'new'"
  } else {
    "Illegal constructor"
  };
  out.push_str(&format!(
    "  Err(rt.throw_type_error({msg_lit}))\n",
    msg_lit = rust_string_literal(call_without_new_message)
  ));
  out.push_str("}\n\n");

  let construct_fn_name = ctor_construct_fn_name(interface);
  let host_ident = "host";
  let args_ident = if overloads.len() == 1 && overloads[0].arguments.is_empty() {
    "_args"
  } else {
    "args"
  };
  out.push_str(&format!(
    "#[allow(dead_code)]\nfn {construct_fn_name}(\n  vm: &mut Vm,\n  scope: &mut Scope<'_>,\n  {host_ident}: &mut dyn VmHost,\n  hooks: &mut dyn VmHostHooks,\n  callee: GcObject,\n  {args_ident}: &[Value],\n  new_target: Value,\n) -> Result<Value, VmError>\n{{\n",
  ));
  out.push_str("  let mut rt = BindingsRuntime::from_scope(vm, scope.reborrow());\n");
  out.push_str("  let rt = &mut rt;\n");
  if overloads.is_empty() {
    if interface == "Document" {
      out.push_str("  let _ = (callee, args, new_target);\n\n");
      out.push_str(
        "  // `new Document()` is a historical extension supported by browsers; it creates a detached,\n  // windowless XML document.\n  //\n  // Delegate to the existing `document.implementation.createDocument(null, null, null)` native\n  // path so we reuse the realm-owned document infrastructure (wrapper setup, owned `dom2::Document`\n  // storage, adoption semantics, etc).\n",
      );
      out.push_str(
        "  let document_obj = rt\n    .vm\n    .user_data::<crate::js::window_realm::WindowRealmUserData>()\n    .and_then(|data| data.document_obj())\n    .ok_or_else(|| rt.throw_type_error(\"Illegal invocation\"))?;\n\n",
      );
      out.push_str("  // Root `document_obj` while allocating property keys.\n");
      out.push_str("  rt.scope.push_root(Value::Object(document_obj))?;\n");
      out.push_str("  let implementation_key = rt.property_key(\"implementation\")?;\n");
      out.push_str("  let implementation_v = rt\n    .vm\n    .get_with_host_and_hooks(host, &mut rt.scope, hooks, document_obj, implementation_key)?;\n");
      out.push_str("  let Value::Object(impl_obj) = implementation_v else {\n");
      out.push_str(
        "    return Err(rt.throw_type_error(\n      \"Document constructor requires document.implementation\",\n    ));\n  };\n\n",
      );
      out.push_str("  // Root while resolving + calling `createDocument`.\n");
      out.push_str("  rt.scope.push_root(Value::Object(impl_obj))?;\n");
      out.push_str("  let create_document_key = rt.property_key(\"createDocument\")?;\n");
      out.push_str(
        "  let create_document_func = rt\n    .vm\n    .get_with_host_and_hooks(host, &mut rt.scope, hooks, impl_obj, create_document_key)?;\n\n",
      );
      out.push_str(
        "  rt.vm.call_with_host_and_hooks(\n    host,\n    &mut rt.scope,\n    hooks,\n    create_document_func,\n    Value::Object(impl_obj),\n    &[Value::Null, Value::Null, Value::Null],\n  )\n",
      );
      out.push_str("}\n\n");
      return;
    }
    out.push_str("  let _ = (host, hooks, callee, args, new_target);\n");
    out.push_str("  Err(rt.throw_type_error(\"Illegal constructor\"))\n");
    out.push_str("}\n\n");
    return;
  }
  out.push_str(&format!(
    "  let default_proto = rt.require_native_object_slot(callee, 0, {msg_lit})?;\n",
    msg_lit = rust_string_literal(&format!("{interface} constructor missing prototype slot"))
  ));
  out.push_str(
    "  let wrapper_proto = rt.derive_prototype_from_new_target(host, hooks, default_proto, new_target)?;\n",
  );
  out.push_str("  let obj = rt.alloc_object_with_prototype(Some(wrapper_proto))?;\n\n");

  if overloads.len() == 1 {
    out.push_str(&emit_ctor_overload_call_vmjs(
      resolved,
      interface,
      0,
      &overloads[0].arguments,
    ));
    out.push_str("}\n\n");
    return;
  }

  // Like operations, constructor overload resolution clamps `arguments.length` to the maximum
  // argument count in the overload set (unless the set includes a variadic overload).
  let max_argc = overloads
    .iter()
    .map(|sig| max_arg_count(&sig.arguments))
    .collect::<Vec<_>>();
  if max_argc.iter().all(|v| v.is_some()) {
    let max_argc = max_argc.into_iter().flatten().max().unwrap_or(0);
    out.push_str(&format!(
      "  let args = if args.len() > {max_argc} {{ &args[..{max_argc}] }} else {{ args }};\n",
      max_argc = max_argc
    ));
  }

  for (idx, sig) in overloads.iter().enumerate() {
    let required = required_arg_count(&sig.arguments);
    let max = max_arg_count(&sig.arguments);
    let cond = emit_args_len_check("args", required, max);
    if idx == 0 {
      out.push_str(&format!("  if {cond} {{\n"));
    } else {
      out.push_str(&format!("  }} else if {cond} {{\n"));
    }
    out.push_str(&indent_lines(
      &emit_ctor_overload_call_vmjs(resolved, interface, idx, &sig.arguments),
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

fn emit_ctor_overload_call_vmjs(
  resolved: &ResolvedWebIdlWorld,
  interface: &str,
  overload_idx: usize,
  arguments: &[Argument],
) -> String {
  let mut out = String::new();
  out.push_str("  {\n");
  if arguments.is_empty() {
    out.push_str("    let converted_args: Vec<Value> = Vec::new();\n");
  } else {
    out.push_str("    let mut converted_args: Vec<Value> = Vec::new();\n");
  }

  for (idx, arg) in arguments.iter().enumerate() {
    if arg.variadic {
      out.push_str(&format!(
        "    for v in args.iter().copied().skip({idx}) {{\n      let converted = {};\n      let converted = rt.scope.push_root(converted)?;\n      converted_args.push(converted);\n    }}\n",
        emit_conversion_expr_vmjs(resolved, &arg.type_, "v", true)
      ));
      break;
    }

    out.push_str(&format!(
      "    let v{idx} = if args.len() > {idx} {{ args[{idx}] }} else {{ Value::Undefined }};\n",
      idx = idx
    ));
    let expr = emit_conversion_expr_for_optional_vmjs(resolved, arg, &format!("v{idx}"), true);
    out.push_str(&format!("    let converted = {expr};\n"));
    out.push_str("    let converted = rt.scope.push_root(converted)?;\n");
    out.push_str("    converted_args.push(converted);\n");
  }

  out.push_str("    let bindings_host = host_from_hooks(hooks)?;\n");
  out.push_str(&format!(
    "    let _ = bindings_host.call_operation(&mut *rt.vm, &mut rt.scope, Some(Value::Object(obj)), {iface_lit}, \"constructor\", {overload_idx}, &converted_args)?;\n",
    iface_lit = rust_string_literal(interface),
    overload_idx = overload_idx,
  ));
  out.push_str("    Ok(Value::Object(obj))\n");
  out.push_str("  }\n");
  out
}

fn emit_conversion_expr_for_optional_vmjs(
  resolved: &ResolvedWebIdlWorld,
  arg: &Argument,
  value_ident: &str,
  rt_is_ref: bool,
) -> String {
  let is_optional = arg.optional || arg.default.is_some();
  if !is_optional {
    return emit_conversion_expr_vmjs(resolved, &arg.type_, value_ident, rt_is_ref);
  }

  // Dictionary arguments with defaults (e.g. `options = {}`): WebIDL still runs dictionary
  // conversion on the default value so member defaults / required-member checks are applied.
  //
  // Our generic optional/default handling returns the default literal directly (skipping conversion),
  // so defaulted dictionaries need a special-case.
  if let IdlType::Named(name) = &arg.type_ {
    if resolved.dictionaries.contains_key(name) {
      if let Some(default) = arg.default.as_ref() {
        let rt_expr = if rt_is_ref { "rt" } else { "&mut rt" };
        let default_expr = emit_default_literal_vmjs(default);
        let dict_fn = to_snake_ident(name);
        return format!(
          "if matches!({value_ident}, Value::Undefined) {{ let default_value = {default_expr}; js_to_dict_{dict_fn}({rt_expr}, host, hooks, default_value)? }} else {{ js_to_dict_{dict_fn}({rt_expr}, host, hooks, {value_ident})? }}"
        );
      }
    }
  }

  let default_expr = arg
    .default
    .as_ref()
    .map(emit_default_literal_vmjs)
    .unwrap_or_else(|| "Value::Undefined".to_string());

  format!(
    "if matches!({value_ident}, Value::Undefined) {{ {default_expr} }} else {{ {} }}",
    emit_conversion_expr_vmjs(resolved, &arg.type_, value_ident, rt_is_ref),
  )
}

fn emit_default_literal_vmjs(lit: &IdlLiteral) -> String {
  match lit {
    IdlLiteral::Undefined => "Value::Undefined".to_string(),
    IdlLiteral::Null => "Value::Null".to_string(),
    IdlLiteral::Boolean(b) => format!("Value::Bool({})", if *b { "true" } else { "false" }),
    IdlLiteral::Number(n) => {
      if let Ok(v) = n.parse::<f64>() {
        format!("Value::Number({v:?})")
      } else {
        "Value::Number(0.0)".to_string()
      }
    }
    IdlLiteral::String(s) => {
      format!(
        "Value::String(rt.alloc_string({})?)",
        rust_string_literal(s)
      )
    }
    IdlLiteral::EmptyObject => "{ let obj = rt.alloc_object()?; Value::Object(obj) }".to_string(),
    IdlLiteral::EmptyArray => "{ let arr = rt.alloc_array(0)?; Value::Object(arr) }".to_string(),
    IdlLiteral::Identifier(_id) => "Value::Undefined".to_string(),
  }
}

fn emit_constant_value_expr_vmjs(lit: &IdlLiteral) -> String {
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
    let (radix, digits) =
      if let Some(hex) = rest.strip_prefix("0x").or_else(|| rest.strip_prefix("0X")) {
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

  fn render_f64_expr(v: f64) -> String {
    if v.is_nan() {
      "f64::NAN".to_string()
    } else if v == f64::INFINITY {
      "f64::INFINITY".to_string()
    } else if v == f64::NEG_INFINITY {
      "f64::NEG_INFINITY".to_string()
    } else {
      format!("{v:?}")
    }
  }

  match lit {
    IdlLiteral::Undefined => "Value::Undefined".to_string(),
    IdlLiteral::Null => "Value::Null".to_string(),
    IdlLiteral::Boolean(b) => format!("Value::Bool({})", if *b { "true" } else { "false" }),
    IdlLiteral::Number(n) => {
      let v = parse_idl_number_literal(n).unwrap_or(0.0);
      format!("Value::Number({})", render_f64_expr(v))
    }
    IdlLiteral::String(s) => format!(
      "Value::String(rt.alloc_string({})?)",
      rust_string_literal(s)
    ),
    IdlLiteral::EmptyObject | IdlLiteral::EmptyArray | IdlLiteral::Identifier(_) => {
      "Value::Undefined".to_string()
    }
  }
}

fn emit_conversion_expr_vmjs(
  resolved: &ResolvedWebIdlWorld,
  ty: &IdlType,
  value_ident: &str,
  rt_is_ref: bool,
) -> String {
  match ty {
    IdlType::Annotated { ext_attrs, inner } => {
      let inner_expr = emit_conversion_expr_vmjs(resolved, inner, value_ident, rt_is_ref);
      let legacy_null_to_empty = ext_attrs
        .iter()
        .any(|attr| attr.name == "LegacyNullToEmptyString");
      if legacy_null_to_empty
        && matches!(
          inner.as_ref(),
          IdlType::Builtin(BuiltinType::DOMString | BuiltinType::USVString | BuiltinType::ByteString)
        )
      {
        // WebIDL `[LegacyNullToEmptyString]` conversion: treat `null` and `undefined` as "" before
        // running the usual string conversion.
        //
        // Important: in `vm-js`-style bindings we do this before host dispatch so the embedder sees
        // the correct value even when the generated setter eagerly converts arguments.
        return format!(
          "if matches!({value_ident}, Value::Null | Value::Undefined) {{ Value::String(rt.alloc_string(\"\")?) }} else {{ {inner_expr} }}",
          value_ident = value_ident,
          inner_expr = inner_expr
        );
      }
      inner_expr
    }
    IdlType::Builtin(b) => match b {
      BuiltinType::Undefined => "Value::Undefined".to_string(),
      BuiltinType::Any | BuiltinType::Object => value_ident.to_string(),
      BuiltinType::Boolean => {
        format!("Value::Bool(rt.scope.heap().to_boolean({value_ident})?)")
      }
      BuiltinType::DOMString | BuiltinType::USVString | BuiltinType::ByteString => {
        format!(
          "Value::String(rt.scope.to_string(&mut *rt.vm, host, hooks, {value_ident})?)"
        )
      }
      BuiltinType::Long => format!(
        "Value::Number(to_int32_f64(rt.scope.to_number(&mut *rt.vm, host, hooks, {value_ident})?) as f64)"
      ),
      BuiltinType::UnsignedLong => format!(
        "Value::Number(to_uint32_f64(rt.scope.to_number(&mut *rt.vm, host, hooks, {value_ident})?) as f64)"
      ),
      BuiltinType::Byte
      | BuiltinType::Octet
      | BuiltinType::Short
      | BuiltinType::UnsignedShort
      | BuiltinType::LongLong
      | BuiltinType::UnsignedLongLong
      | BuiltinType::Float
      | BuiltinType::UnrestrictedFloat
      | BuiltinType::Double
      | BuiltinType::UnrestrictedDouble => {
        format!(
          "Value::Number(rt.scope.to_number(&mut *rt.vm, host, hooks, {value_ident})?)"
        )
      }
    },
    IdlType::Named(name) => {
      if resolved.dictionaries.contains_key(name) {
        let rt_expr = if rt_is_ref { "rt" } else { "&mut rt" };
        format!(
          "js_to_dict_{}({rt_expr}, host, hooks, {value_ident})?",
          to_snake_ident(name)
        )
      } else if let Some(en) = resolved.enums.get(name) {
        let allowed = en
          .values
          .iter()
          .map(|v| rust_string_literal(v))
          .collect::<Vec<_>>()
          .join(", ");
        let rt_expr = if rt_is_ref { "rt" } else { "&mut rt" };
        format!(
          "conversions::to_enum({rt_expr}, host, hooks, {value_ident}, {enum_name}, &[{allowed}])?",
          rt_expr = rt_expr,
          value_ident = value_ident,
          enum_name = rust_string_literal(name),
          allowed = allowed,
        )
      } else if name == "Function" {
        // WebIDL uses the `Function` interface type for "callable objects". Treat it like a
        // callback function conversion so non-callables throw a TypeError.
        let rt_expr = if rt_is_ref { "rt" } else { "&mut rt" };
        format!(
          "conversions::to_callback_function({rt_expr}, host, hooks, {value_ident})?",
          rt_expr = rt_expr,
          value_ident = value_ident
        )
      } else if resolved.callbacks.contains_key(name) {
        let rt_expr = if rt_is_ref { "rt" } else { "&mut rt" };
        format!(
          "conversions::to_callback_function({rt_expr}, host, hooks, {value_ident})?",
          rt_expr = rt_expr,
          value_ident = value_ident
        )
      } else if resolved
        .interfaces
        .get(name)
        .is_some_and(|iface| iface.callback)
      {
        let rt_expr = if rt_is_ref { "rt" } else { "&mut rt" };
        format!(
          "conversions::to_callback_interface({rt_expr}, host, hooks, {value_ident})?",
          rt_expr = rt_expr,
          value_ident = value_ident
        )
      } else if resolved.typedefs.contains_key(name) {
        match resolved.resolve_typedef(name) {
          Ok(expanded) => emit_conversion_expr_vmjs(resolved, &expanded, value_ident, rt_is_ref),
          Err(_) => value_ident.to_string(),
        }
      } else {
        value_ident.to_string()
      }
    }
    IdlType::Nullable(inner) => format!(
      "if matches!({value_ident}, Value::Null | Value::Undefined) {{ Value::Null }} else {{ {} }}",
      emit_conversion_expr_vmjs(resolved, inner, value_ident, rt_is_ref)
    ),
    IdlType::Union(members) => emit_union_conversion_expr_vmjs(resolved, members, value_ident, rt_is_ref),
    IdlType::Sequence(elem) => {
      emit_iterable_list_conversion_expr_vmjs(resolved, elem, value_ident, "sequence", rt_is_ref)
    }
    IdlType::FrozenArray(elem) => {
      emit_iterable_list_conversion_expr_vmjs(resolved, elem, value_ident, "FrozenArray", rt_is_ref)
    }
    IdlType::Promise(_) => value_ident.to_string(),
    IdlType::Record { key, value } => {
      emit_record_conversion_expr_vmjs(resolved, key, value, value_ident, rt_is_ref)
    }
  }
}

fn emit_union_conversion_expr_vmjs(
  resolved: &ResolvedWebIdlWorld,
  members: &[IdlType],
  value_ident: &str,
  rt_is_ref: bool,
) -> String {
  fn push_expanded_union_members(ty: IdlType, out: &mut Vec<IdlType>) {
    match ty {
      IdlType::Union(members) => {
        for member in members {
          push_expanded_union_members(member, out);
        }
      }
      IdlType::Nullable(inner) => match *inner {
        IdlType::Union(members) => {
          for member in members {
            push_expanded_union_members(IdlType::Nullable(Box::new(member)), out);
          }
        }
        other => out.push(IdlType::Nullable(Box::new(other))),
      },
      other => out.push(other),
    }
  }

  // Expand any typedefs referenced inside the union. This ensures union members like
  // `Foo = (DOMString or Callback)` participate in discrimination (rather than being treated as an
  // opaque object).
  let mut expanded_members: Vec<IdlType> = Vec::new();
  for member in members {
    let expanded = member
      .canonicalize(resolved)
      .unwrap_or_else(|_| member.clone());
    push_expanded_union_members(expanded, &mut expanded_members);
  }

  let mut has_undefined = false;
  let mut has_nullable = false;
  let mut has_any = false;
  let mut has_object = false;

  let mut sequence_member: Option<&IdlType> = None;
  let mut dict_member: Option<&String> = None;
  let mut record_member: Option<&IdlType> = None;
  let mut callback_function_member: Option<&IdlType> = None;
  let mut callback_interface_member: Option<&IdlType> = None;
  let mut boolean_member: Option<&IdlType> = None;
  let mut numeric_member: Option<&IdlType> = None;
  let mut string_member: Option<&IdlType> = None;

  for member in &expanded_members {
    // Unwrap any leading annotations for type discrimination; keep `member` intact so conversion
    // expressions can still see the annotations.
    let mut kind: &IdlType = member;
    while let IdlType::Annotated { inner, .. } = kind {
      kind = inner;
    }

    if let IdlType::Nullable(t) = kind {
      has_nullable = true;
      kind = t;
      while let IdlType::Annotated { inner, .. } = kind {
        kind = inner;
      }
    }

    match kind {
      IdlType::Builtin(BuiltinType::Undefined) => has_undefined = true,
      IdlType::Builtin(BuiltinType::Any) => has_any = true,
      IdlType::Builtin(BuiltinType::Object) => has_object = true,
      IdlType::Builtin(BuiltinType::Boolean) => {
        let _ = boolean_member.get_or_insert(member);
      }
      IdlType::Builtin(
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
        | BuiltinType::UnrestrictedDouble,
      ) => {
        let _ = numeric_member.get_or_insert(member);
      }
      IdlType::Builtin(
        BuiltinType::DOMString | BuiltinType::USVString | BuiltinType::ByteString,
      ) => {
        let _ = string_member.get_or_insert(member);
      }
      IdlType::Named(name) => {
        if resolved.dictionaries.contains_key(name) {
          let _ = dict_member.get_or_insert(name);
        } else if resolved.enums.contains_key(name) {
          let _ = string_member.get_or_insert(member);
        } else if name == "Function" || resolved.callbacks.contains_key(name) {
          let _ = callback_function_member.get_or_insert(member);
        } else if resolved
          .interfaces
          .get(name)
          .is_some_and(|iface| iface.callback)
        {
          let _ = callback_interface_member.get_or_insert(member);
        } else {
          // For now treat unknown/unsupported named types as opaque objects.
          has_object = true;
        }
      }
      IdlType::Sequence(_) | IdlType::FrozenArray(_) => {
        let _ = sequence_member.get_or_insert(member);
      }
      IdlType::Record { .. } => {
        let _ = record_member.get_or_insert(member);
      }
      IdlType::Union(_)
      | IdlType::Promise(_)
      | IdlType::Nullable(_)
      | IdlType::Annotated { .. } => {}
    }
  }

  let dict_expr = dict_member.map(|dict| {
    let rt_expr = if rt_is_ref { "rt" } else { "&mut rt" };
    format!(
      "js_to_dict_{}({rt_expr}, host, hooks, v)?",
      to_snake_ident(dict)
    )
  });
  let rt_expr = if rt_is_ref { "rt" } else { "&mut rt" };
  // When discriminating unions that include a `sequence<>`/`FrozenArray<>` member, WebIDL performs
  // `GetMethod(V, @@iterator)` once and then consumes the iterator with
  // `GetIteratorFromMethod(V, method)`. Avoid double-evaluating `@@iterator` getters/traps by
  // plumbing the resolved method into the conversion.
  let seq_expr = sequence_member.map(|ty| {
    emit_iterable_list_conversion_expr_vmjs_from_method(resolved, ty, "v", "iter_method", rt_is_ref)
  });
  let record_expr = record_member.map(|ty| emit_conversion_expr_vmjs(resolved, ty, "v", rt_is_ref));
  let callback_expr =
    callback_function_member.map(|ty| emit_conversion_expr_vmjs(resolved, ty, "v", rt_is_ref));
  let callback_iface_expr =
    callback_interface_member.map(|ty| emit_conversion_expr_vmjs(resolved, ty, "v", rt_is_ref));
  let boolean_expr =
    boolean_member.map(|ty| emit_conversion_expr_vmjs(resolved, ty, "v", rt_is_ref));
  let numeric_expr =
    numeric_member.map(|ty| emit_conversion_expr_vmjs(resolved, ty, "v", rt_is_ref));
  let string_expr = string_member.map(|ty| emit_conversion_expr_vmjs(resolved, ty, "v", rt_is_ref));

  let mut out = String::new();
  out.push_str("{\n");
  out.push_str(&format!(
    "  let v = {value_ident};\n",
    value_ident = value_ident
  ));

  // Undefined member special-case.
  if has_undefined {
    out.push_str("  if matches!(v, Value::Undefined) {\n    Value::Undefined\n  }");
  } else {
    out.push_str("  if false {\n    Value::Undefined\n  }");
  }

  // `null`/`undefined` dictionary special-case (dictionary converters treat them as "missing").
  if let Some(dict_expr) = &dict_expr {
    out.push_str(" else if matches!(v, Value::Null | Value::Undefined) {\n    ");
    out.push_str(dict_expr);
    out.push_str("\n  }");
  }

  // Nullable special-case.
  if has_nullable {
    out.push_str(" else if matches!(v, Value::Null | Value::Undefined) {\n    Value::Null\n  }");
  }

  // Callback function special-case.
  if let Some(callback_expr) = &callback_expr {
    out.push_str(" else if rt.scope.heap().is_callable(v)? {\n    ");
    out.push_str(callback_expr);
    out.push_str("\n  }");
  }

  let emit_object_discrimination = |out: &mut String, indent: &str| {
    if let Some(seq_expr) = &seq_expr {
      out.push_str(indent);
      out.push_str(&format!(
        "let iter_method = conversions::get_iterator_method({rt_expr}, host, hooks, obj)?;\n",
        rt_expr = rt_expr
      ));

      out.push_str(indent);
      out.push_str("if let Some(iter_method) = iter_method {\n");
      out.push_str(indent);
      out.push_str("  ");
      out.push_str(seq_expr);
      out.push_str("\n");
      out.push_str(indent);
      out.push_str("}");

      // Dictionary/record should only be considered when the object is not iterable.
      if let Some(dict_expr) = &dict_expr {
        out.push_str(" else {\n");
        out.push_str(indent);
        out.push_str("  ");
        out.push_str(dict_expr);
        out.push_str("\n");
        out.push_str(indent);
        out.push_str("}");
      } else if let Some(record_expr) = &record_expr {
        out.push_str(" else {\n");
        out.push_str(indent);
        out.push_str("  ");
        out.push_str(record_expr);
        out.push_str("\n");
        out.push_str(indent);
        out.push_str("}");
      } else if let Some(callback_iface_expr) = &callback_iface_expr {
        out.push_str(" else {\n");
        out.push_str(indent);
        out.push_str("  ");
        out.push_str(callback_iface_expr);
        out.push_str("\n");
        out.push_str(indent);
        out.push_str("}");
      } else if has_object || has_any {
        out.push_str(" else {\n");
        out.push_str(indent);
        out.push_str("  v\n");
        out.push_str(indent);
        out.push_str("}");
      } else {
        out.push_str(" else {\n");
        out.push_str(indent);
        out.push_str("  return Err(rt.throw_type_error(\"Value is not a valid union type\"));\n");
        out.push_str(indent);
        out.push_str("}");
      }
      return;
    }

    out.push_str(indent);
    if let Some(dict_expr) = &dict_expr {
      out.push_str(dict_expr);
    } else if let Some(record_expr) = &record_expr {
      out.push_str(record_expr);
    } else if let Some(callback_iface_expr) = &callback_iface_expr {
      out.push_str(callback_iface_expr);
    } else if has_object || has_any {
      out.push_str("v");
    } else {
      out.push_str("return Err(rt.throw_type_error(\"Value is not a valid union type\"));");
    }
  };

  // Object branch: sequence/record/dictionary/callback interface/object.
  let needs_obj_binding = seq_expr.is_some() || string_expr.is_some();
  if needs_obj_binding {
    out.push_str(" else if let Value::Object(obj) = v {\n");
  } else {
    out.push_str(" else if let Value::Object(_) = v {\n");
  }

  // WebIDL union conversions treat boxed String objects as strings when the union includes a string
  // type, and this check must occur before iterable/record/dictionary discrimination.
  if let Some(string_expr) = &string_expr {
    out.push_str(&format!(
      "    if conversions::is_string_object({rt_expr}, host, hooks, obj)? {{\n      {string_expr}\n    }} else {{\n",
      rt_expr = rt_expr,
      string_expr = string_expr
    ));
    emit_object_discrimination(&mut out, "      ");
    out.push_str("\n    }\n  }");
  } else {
    emit_object_discrimination(&mut out, "    ");
    out.push_str("\n  }");
  }

  // Primitive fast paths and fallthrough conversions.
  if let Some(boolean_expr) = &boolean_expr {
    out.push_str(" else if matches!(v, Value::Bool(_)) {\n    ");
    out.push_str(boolean_expr);
    out.push_str("\n  }");
  }
  if let Some(numeric_expr) = &numeric_expr {
    out.push_str(" else if matches!(v, Value::Number(_)) {\n    ");
    out.push_str(numeric_expr);
    out.push_str("\n  }");
  }
  if let Some(string_expr) = &string_expr {
    out.push_str(" else if matches!(v, Value::String(_)) {\n    ");
    out.push_str(string_expr);
    out.push_str("\n  }");
  }

  out.push_str(" else {\n    ");
  if let Some(string_expr) = &string_expr {
    out.push_str(string_expr);
    out.push_str("\n  }\n");
  } else if let Some(numeric_expr) = &numeric_expr {
    out.push_str(numeric_expr);
    out.push_str("\n  }\n");
  } else if let Some(boolean_expr) = &boolean_expr {
    out.push_str(boolean_expr);
    out.push_str("\n  }\n");
  } else if has_any {
    out.push_str("v\n  }\n");
  } else {
    out.push_str("return Err(rt.throw_type_error(\"Value is not a valid union type\"));\n  }\n");
  }

  out.push_str("}\n");
  out
}

fn emit_iterable_list_conversion_expr_vmjs(
  resolved: &ResolvedWebIdlWorld,
  elem_ty: &IdlType,
  value_ident: &str,
  kind_label: &str,
  rt_is_ref: bool,
) -> String {
  let elem_expr = emit_conversion_expr_vmjs(resolved, elem_ty, "next", rt_is_ref);
  let expected_msg = rust_string_literal(&format!("expected object for {kind_label}"));
  let rt_expr = if rt_is_ref { "rt" } else { "&mut rt" };
  format!(
    "conversions::to_iterable_list({rt_expr}, host, hooks, {value_ident}, {expected_msg}, |rt, host, hooks, next| Ok({elem_expr}))?",
    rt_expr = rt_expr,
    value_ident = value_ident,
    expected_msg = expected_msg,
    elem_expr = elem_expr
  )
}

fn emit_iterable_list_conversion_expr_vmjs_from_method(
  resolved: &ResolvedWebIdlWorld,
  ty: &IdlType,
  value_ident: &str,
  method_ident: &str,
  rt_is_ref: bool,
) -> String {
  match ty {
    IdlType::Annotated { inner, .. } => emit_iterable_list_conversion_expr_vmjs_from_method(
      resolved,
      inner,
      value_ident,
      method_ident,
      rt_is_ref,
    ),
    IdlType::Nullable(inner) => format!(
      "if matches!({value_ident}, Value::Null | Value::Undefined) {{ Value::Null }} else {{ {} }}",
      emit_iterable_list_conversion_expr_vmjs_from_method(
        resolved,
        inner,
        value_ident,
        method_ident,
        rt_is_ref
      )
    ),
    IdlType::Sequence(elem_ty) => {
      let elem_expr = emit_conversion_expr_vmjs(resolved, elem_ty, "next", rt_is_ref);
      let expected_msg = rust_string_literal("expected object for sequence");
      let rt_expr = if rt_is_ref { "rt" } else { "&mut rt" };
      format!(
        "conversions::to_iterable_list_from_method({rt_expr}, host, hooks, {value_ident}, {method_ident}, {expected_msg}, |rt, host, hooks, next| Ok({elem_expr}))?",
        rt_expr = rt_expr,
        value_ident = value_ident,
        method_ident = method_ident,
        expected_msg = expected_msg,
        elem_expr = elem_expr,
      )
    }
    IdlType::FrozenArray(elem_ty) => {
      let elem_expr = emit_conversion_expr_vmjs(resolved, elem_ty, "next", rt_is_ref);
      let expected_msg = rust_string_literal("expected object for FrozenArray");
      let rt_expr = if rt_is_ref { "rt" } else { "&mut rt" };
      format!(
        "conversions::to_iterable_list_from_method({rt_expr}, host, hooks, {value_ident}, {method_ident}, {expected_msg}, |rt, host, hooks, next| Ok({elem_expr}))?",
        rt_expr = rt_expr,
        value_ident = value_ident,
        method_ident = method_ident,
        expected_msg = expected_msg,
        elem_expr = elem_expr,
      )
    }
    // This helper is only used by union discrimination for `sequence<>`/`FrozenArray<>` members.
    // Emit a regular conversion expression for anything else.
    _ => emit_conversion_expr_vmjs(resolved, ty, value_ident, rt_is_ref),
  }
}

fn emit_record_conversion_expr_vmjs(
  resolved: &ResolvedWebIdlWorld,
  key_ty: &IdlType,
  value_ty: &IdlType,
  value_ident: &str,
  rt_is_ref: bool,
) -> String {
  let _ = key_ty;
  let value_expr = emit_conversion_expr_vmjs(resolved, value_ty, "prop_value", rt_is_ref);
  let expected_msg = rust_string_literal("expected object for record");
  let rt_expr = if rt_is_ref { "rt" } else { "&mut rt" };
  format!(
    "conversions::to_record({rt_expr}, host, hooks, {value_ident}, {expected_msg}, |rt, host, hooks, prop_value| Ok({value_expr}))?",
    rt_expr = rt_expr,
    value_ident = value_ident,
    expected_msg = expected_msg,
    value_expr = value_expr
  )
}

fn emit_type_predicate_vmjs(ty: &IdlType, value_expr: &str) -> String {
  match ty {
    IdlType::Annotated { inner, .. } => emit_type_predicate_vmjs(inner, value_expr),
    IdlType::Builtin(b) => match b {
      BuiltinType::Boolean => format!("matches!({value_expr}, Value::Bool(_))"),
      BuiltinType::DOMString | BuiltinType::USVString | BuiltinType::ByteString => {
        format!("matches!({value_expr}, Value::String(_))")
      }
      BuiltinType::Object | BuiltinType::Any => "true".to_string(),
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
      | BuiltinType::UnrestrictedDouble => format!("matches!({value_expr}, Value::Number(_))"),
      BuiltinType::Undefined => format!("matches!({value_expr}, Value::Undefined)"),
    },
    IdlType::Named(_name) => format!("matches!({value_expr}, Value::Object(_))"),
    IdlType::Nullable(inner) => format!(
      "matches!({value_expr}, Value::Null) || ({})",
      emit_type_predicate_vmjs(inner, value_expr)
    ),
    IdlType::Union(_)
    | IdlType::Sequence(_)
    | IdlType::FrozenArray(_)
    | IdlType::Promise(_)
    | IdlType::Record { .. } => "true".to_string(),
  }
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

fn ctor_call_without_new_fn_name(interface: &str) -> String {
  format!("{}_call_without_new", to_snake_ident(interface))
}

fn ctor_construct_fn_name(interface: &str) -> String {
  format!("{}_construct", to_snake_ident(interface))
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

fn to_snake_public_ident(name: &str) -> String {
  // Like `to_snake_ident`, but handle consecutive uppercase sequences more naturally so
  // `URLSearchParams` becomes `url_search_params` (rather than `u_r_l_search_params`).
  let chars: Vec<char> = name.chars().collect();
  let mut out = String::new();

  for (i, ch) in chars.iter().copied().enumerate() {
    if ch == '-' {
      out.push('_');
      continue;
    }

    if ch.is_ascii_uppercase() {
      let prev = chars.get(i.wrapping_sub(1)).copied();
      let next = chars.get(i + 1).copied();

      let prev_is_lower_or_digit =
        prev.is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit());
      let prev_is_upper = prev.is_some_and(|c| c.is_ascii_uppercase());
      let next_is_lower = next.is_some_and(|c| c.is_ascii_lowercase());

      if i != 0 && (prev_is_lower_or_digit || (prev_is_upper && next_is_lower)) {
        out.push('_');
      }
      out.push(ch.to_ascii_lowercase());
      continue;
    }

    out.push(ch);
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
