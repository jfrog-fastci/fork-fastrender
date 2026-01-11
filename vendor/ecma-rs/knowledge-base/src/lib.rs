use std::collections::BTreeMap;
use std::fmt;

use effect_model::{EffectFlags, EffectSummary, EffectTemplate, PurityTemplate, ThrowBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiSemantics {
  pub name: String,

  #[serde(default)]
  pub aliases: Vec<String>,

  #[serde(default)]
  pub effects: EffectTemplate,

  #[serde(default)]
  pub purity: PurityTemplate,

  #[serde(default, rename = "async", skip_serializing_if = "Option::is_none")]
  pub async_: Option<bool>,

  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub idempotent: Option<bool>,

  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub deterministic: Option<bool>,

  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub parallelizable: Option<bool>,

  /// Arbitrary key/value metadata for downstream analyses.
  ///
  /// `effect-js` uses this for optional string encoding semantics such as:
  /// - `encoding.output: same_as_input`
  /// - `encoding.preserves_input_if: ascii`
  /// - `encoding.length_preserving_if: ascii`
  ///
  /// Values are strings to keep the schema stable and easy to author.
  #[serde(default)]
  pub properties: BTreeMap<String, JsonValue>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApiDatabase {
  apis: BTreeMap<String, ApiSemantics>,
}

/// Backwards-compatible alias used by analysis passes.
pub type KnowledgeBase = ApiDatabase;

/// Backwards-compatible name used by `effect-js` query helpers.
pub type Api = ApiSemantics;

impl ApiDatabase {
  pub fn from_entries(entries: impl IntoIterator<Item = ApiSemantics>) -> Self {
    let mut apis = BTreeMap::new();
    for api in entries {
      apis.insert(api.name.clone(), api);
    }
    Self { apis }
  }

  pub fn get(&self, name: &str) -> Option<&ApiSemantics> {
    self.apis.get(name)
  }

  pub fn iter(&self) -> impl Iterator<Item = (&str, &ApiSemantics)> {
    self.apis.iter().map(|(k, v)| (k.as_str(), v))
  }

  /// Load the bundled knowledge base embedded into the crate.
  ///
  /// This is a convenience alias for [`ApiDatabase::load_default`].
  pub fn from_embedded() -> Result<Self, KnowledgeBaseError> {
    Self::load_default()
  }

  pub fn load_default() -> Result<Self, KnowledgeBaseError> {
    let mut apis = BTreeMap::<String, ApiSemantics>::new();
    let mut sources = BTreeMap::<String, String>::new();

    for file in bundled_kb::BUNDLED_KB_FILES {
      let parsed = parse_bundled_file(file)?;
      for api in parsed {
        if let Some(prev) = sources.get(&api.name) {
          return Err(KnowledgeBaseError::DuplicateApi {
            name: api.name,
            first: prev.clone(),
            second: file.path.to_string(),
          });
        }
        sources.insert(api.name.clone(), file.path.to_string());
        apis.insert(api.name.clone(), api);
      }
    }

    Ok(Self { apis })
  }

  pub fn validate(&self) -> Result<(), KnowledgeBaseError> {
    build_alias_map(&self.apis)?;
    self.warn_inconsistent_metadata();
    Ok(())
  }

  fn warn_inconsistent_metadata(&self) {
    for api in self.apis.values() {
      if api.deterministic == Some(true) && api.purity == PurityTemplate::Impure {
        tracing::warn!(
          api = api.name,
          "sanity check: deterministic=true but purity.template=impure (likely inconsistent)"
        );
      }
      if api.async_ == Some(true) && api.purity == PurityTemplate::Pure {
        tracing::warn!(
          api = api.name,
          "sanity check: async=true but purity.template=pure (rare; verify intent)"
        );
      }
    }
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundledKbFormat {
  Yaml,
  Toml,
}

impl fmt::Display for BundledKbFormat {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Yaml => f.write_str("YAML"),
      Self::Toml => f.write_str("TOML"),
    }
  }
}

#[derive(Debug, Clone, Copy)]
struct BundledKbFile {
  path: &'static str,
  format: BundledKbFormat,
  contents: &'static str,
}

mod bundled_kb {
  use super::{BundledKbFile, BundledKbFormat};
  include!(concat!(env!("OUT_DIR"), "/bundled_kb.rs"));
}

#[derive(Debug, thiserror::Error)]
pub enum KnowledgeBaseError {
  #[error("failed to parse knowledge base file `{path}` as {format}: {source}")]
  Parse {
    path: String,
    format: BundledKbFormat,
    #[source]
    source: Box<dyn std::error::Error + 'static>,
  },

  #[error("knowledge base file `{path}` declares unsupported schema version {schema}")]
  UnsupportedSchema { path: String, schema: u32 },

  #[error("duplicate API name `{name}` in `{first}` and `{second}`")]
  DuplicateApi {
    name: String,
    first: String,
    second: String,
  },

  #[error("duplicate alias `{alias}` for `{first}` and `{second}`")]
  DuplicateAlias {
    alias: String,
    first: String,
    second: String,
  },
}

#[derive(Debug, Clone, Deserialize)]
struct ModuleRaw {
  #[serde(alias = "schema_version")]
  schema: u32,

  #[serde(alias = "symbols", default)]
  apis: Vec<ApiRaw>,
}

#[derive(Debug, Clone, Deserialize)]
struct ApiRaw {
  name: String,

  #[serde(default)]
  aliases: Vec<String>,

  #[serde(default)]
  effects: EffectsRaw,

  #[serde(default)]
  purity: PurityRaw,

  #[serde(default)]
  throws: Option<String>,

  #[serde(default, rename = "async")]
  async_: Option<bool>,

  #[serde(default)]
  idempotent: Option<bool>,

  #[serde(default)]
  deterministic: Option<bool>,

  #[serde(default)]
  parallelizable: Option<bool>,

  #[serde(default)]
  properties: BTreeMap<String, JsonValue>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct ApiBodyRaw {
  #[serde(default)]
  aliases: Vec<String>,

  #[serde(default)]
  effects: EffectsRaw,

  #[serde(default)]
  purity: PurityRaw,

  #[serde(default)]
  throws: Option<String>,

  #[serde(default, rename = "async")]
  async_: Option<bool>,

  #[serde(default)]
  idempotent: Option<bool>,

  #[serde(default)]
  deterministic: Option<bool>,

  #[serde(default)]
  parallelizable: Option<bool>,

  #[serde(default)]
  properties: BTreeMap<String, JsonValue>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum EffectsRaw {
  Template(EffectTemplate),
  Details(EffectsDetailsRaw),
}

impl Default for EffectsRaw {
  fn default() -> Self {
    Self::Template(EffectTemplate::Unknown)
  }
}

#[derive(Debug, Clone, Deserialize, Default)]
struct EffectsDetailsRaw {
  #[serde(default)]
  template: Option<String>,

  #[serde(default)]
  may_throw: Option<bool>,

  #[serde(default)]
  allocates: Option<bool>,

  #[serde(default)]
  io: Option<bool>,

  #[serde(default)]
  network: Option<bool>,

  #[serde(default)]
  nondeterministic: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum PurityRaw {
  Template(PurityTemplate),
  Details(PurityDetailsRaw),
}

impl Default for PurityRaw {
  fn default() -> Self {
    Self::Template(PurityTemplate::Unknown)
  }
}

#[derive(Debug, Clone, Deserialize, Default)]
struct PurityDetailsRaw {
  #[serde(default)]
  template: Option<String>,
}

fn normalize_api(raw: ApiRaw) -> ApiSemantics {
  let effects = normalize_effects(raw.effects, raw.throws.as_deref());
  let purity = normalize_purity(raw.purity);
  ApiSemantics {
    name: raw.name,
    aliases: raw.aliases,
    effects,
    purity,
    async_: raw.async_,
    idempotent: raw.idempotent,
    deterministic: raw.deterministic,
    parallelizable: raw.parallelizable,
    properties: raw.properties,
  }
}

fn normalize_api_from_body(name: String, raw: ApiBodyRaw) -> ApiSemantics {
  let effects = normalize_effects(raw.effects, raw.throws.as_deref());
  let purity = normalize_purity(raw.purity);
  ApiSemantics {
    name,
    aliases: raw.aliases,
    effects,
    purity,
    async_: raw.async_,
    idempotent: raw.idempotent,
    deterministic: raw.deterministic,
    parallelizable: raw.parallelizable,
    properties: raw.properties,
  }
}

fn normalize_purity(raw: PurityRaw) -> PurityTemplate {
  match raw {
    PurityRaw::Template(t) => t,
    PurityRaw::Details(details) => match details.template {
      Some(template) => parse_purity_template(&template),
      None => PurityTemplate::Unknown,
    },
  }
}

fn parse_purity_template(raw: &str) -> PurityTemplate {
  match normalize_ident(raw).as_str() {
    "pure" => PurityTemplate::Pure,
    "readonly" | "read_only" => PurityTemplate::ReadOnly,
    "allocating" => PurityTemplate::Allocating,
    "depends_on_callback" => PurityTemplate::DependsOnCallback,
    "impure" => PurityTemplate::Impure,
    "unknown" => PurityTemplate::Unknown,
    _ => PurityTemplate::Unknown,
  }
}

fn normalize_effects(raw: EffectsRaw, throws: Option<&str>) -> EffectTemplate {
  match raw {
    EffectsRaw::Template(t) => t,
    EffectsRaw::Details(details) => {
      let template = details
        .template
        .as_deref()
        .map(normalize_ident)
        .unwrap_or_default();

      // We can't encode "depends on callback + base effects" with the current
      // `effect_model::EffectTemplate`, so preserve the template and conservatively
      // drop the additional booleans.
      if template == "depends_on_callback" {
        return EffectTemplate::DependsOnCallback;
      }

      let unknown_default = template == "unknown";
      let io_default = template == "io";

      let mut flags = EffectFlags::empty();
      if details.allocates.unwrap_or(unknown_default) {
        flags |= EffectFlags::ALLOCATES;
      }
      if details
        .io
        .unwrap_or(io_default || unknown_default)
      {
        flags |= EffectFlags::IO;
      }
      if details.network.unwrap_or(unknown_default) {
        flags |= EffectFlags::NETWORK;
      }
      if details.nondeterministic.unwrap_or(unknown_default) {
        flags |= EffectFlags::NONDETERMINISTIC;
      }

      let throws = match details.may_throw {
        Some(true) => ThrowBehavior::Maybe,
        Some(false) => ThrowBehavior::Never,
        None => throws
          .and_then(parse_throw_behavior)
          .unwrap_or_else(|| {
            if template == "pure" {
              ThrowBehavior::Never
            } else {
              ThrowBehavior::Maybe
            }
          }),
      };

      let summary = EffectSummary { flags, throws };
      if summary.is_pure() {
        return EffectTemplate::Pure;
      }
      if summary.flags == EffectFlags::IO && summary.throws == ThrowBehavior::Maybe {
        return EffectTemplate::Io;
      }
      if summary.flags == EffectFlags::all() && summary.throws == ThrowBehavior::Maybe {
        return EffectTemplate::Unknown;
      }

      EffectTemplate::Custom(summary)
    }
  }
}

fn parse_throw_behavior(raw: &str) -> Option<ThrowBehavior> {
  match normalize_ident(raw).as_str() {
    "never" => Some(ThrowBehavior::Never),
    "maybe" | "unknown" => Some(ThrowBehavior::Maybe),
    "always" => Some(ThrowBehavior::Always),
    _ => None,
  }
}

fn normalize_ident(raw: &str) -> String {
  raw
    .trim()
    .to_ascii_lowercase()
    .replace('-', "_")
    .replace(' ', "_")
}

fn parse_bundled_file(file: &BundledKbFile) -> Result<Vec<ApiSemantics>, KnowledgeBaseError> {
  match file.format {
    BundledKbFormat::Yaml => parse_yaml_file(file),
    BundledKbFormat::Toml => parse_toml_file(file),
  }
}

fn parse_yaml_file(file: &BundledKbFile) -> Result<Vec<ApiSemantics>, KnowledgeBaseError> {
  let value: serde_yaml::Value = serde_yaml::from_str(file.contents).map_err(|err| {
    KnowledgeBaseError::Parse {
      path: file.path.to_string(),
      format: file.format,
      source: Box::new(err),
    }
  })?;

  match value {
    serde_yaml::Value::Sequence(_) => {
      let apis: Vec<ApiRaw> = serde_yaml::from_value(value).map_err(|err| {
        KnowledgeBaseError::Parse {
          path: file.path.to_string(),
          format: file.format,
          source: Box::new(err),
        }
      })?;
      Ok(apis.into_iter().map(normalize_api).collect())
    }
    serde_yaml::Value::Mapping(map) => {
      let is_schema_module = map.contains_key(&serde_yaml::Value::String("schema".to_string()))
        || map.contains_key(&serde_yaml::Value::String("schema_version".to_string()));

      if is_schema_module {
        let module: ModuleRaw = serde_yaml::from_value(serde_yaml::Value::Mapping(map)).map_err(|err| {
          KnowledgeBaseError::Parse {
            path: file.path.to_string(),
            format: file.format,
            source: Box::new(err),
          }
        })?;
        if module.schema != 1 {
          return Err(KnowledgeBaseError::UnsupportedSchema {
            path: file.path.to_string(),
            schema: module.schema,
          });
        }
        Ok(module.apis.into_iter().map(normalize_api).collect())
      } else {
        let apis: BTreeMap<String, ApiBodyRaw> =
          serde_yaml::from_value(serde_yaml::Value::Mapping(map)).map_err(|err| {
            KnowledgeBaseError::Parse {
              path: file.path.to_string(),
              format: file.format,
              source: Box::new(err),
            }
          })?;
        Ok(
          apis
            .into_iter()
            .map(|(name, api)| normalize_api_from_body(name, api))
            .collect(),
        )
      }
    }
    _ => Ok(Vec::new()),
  }
}

fn parse_toml_file(file: &BundledKbFile) -> Result<Vec<ApiSemantics>, KnowledgeBaseError> {
  let module: ModuleRaw = toml::from_str(file.contents).map_err(|err| KnowledgeBaseError::Parse {
    path: file.path.to_string(),
    format: file.format,
    source: Box::new(err),
  })?;
  if module.schema != 1 {
    return Err(KnowledgeBaseError::UnsupportedSchema {
      path: file.path.to_string(),
      schema: module.schema,
    });
  }

  Ok(module.apis.into_iter().map(normalize_api).collect())
}

fn build_alias_map(apis: &BTreeMap<String, ApiSemantics>) -> Result<BTreeMap<String, String>, KnowledgeBaseError> {
  let mut aliases = BTreeMap::<String, String>::new();

  for api in apis.values() {
    for alias in &api.aliases {
      if alias.is_empty() || alias == &api.name {
        continue;
      }

      if let Some(prev) = apis.get(alias) {
        // Some modules materialize alias spellings as standalone entries (e.g. `fs.readFile`)
        // alongside a canonical form (e.g. `node:fs.readFile`) that also lists the alias.
        //
        // When the semantics match, this is redundant but unambiguous: lookups can resolve the
        // alias directly via the entry, so we skip building an alias mapping rather than treating
        // it as an error.
        if prev.effects == api.effects
          && prev.purity == api.purity
          && prev.async_ == api.async_
          && prev.idempotent == api.idempotent
          && prev.deterministic == api.deterministic
          && prev.parallelizable == api.parallelizable
          && prev.properties == api.properties
        {
          continue;
        }

        return Err(KnowledgeBaseError::DuplicateAlias {
          alias: alias.clone(),
          first: prev.name.clone(),
          second: api.name.clone(),
        });
      }

      if let Some(prev) = aliases.insert(alias.clone(), api.name.clone()) {
        return Err(KnowledgeBaseError::DuplicateAlias {
          alias: alias.clone(),
          first: prev,
          second: api.name.clone(),
        });
      }
    }
  }

  Ok(aliases)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
enum ApiSemanticsFile {
  One(ApiSemantics),
  Many(Vec<ApiSemantics>),
}

/// Parse an "entries file" containing either:
///
/// - a single mapping (`name: ...`) representing one [`ApiSemantics`], or
/// - a YAML list of [`ApiSemantics`] objects.
pub fn parse_api_semantics_yaml_str(yaml: &str) -> Result<Vec<ApiSemantics>, serde_yaml::Error> {
  let file: ApiSemanticsFile = serde_yaml::from_str(yaml)?;
  Ok(match file {
    ApiSemanticsFile::One(one) => vec![one],
    ApiSemanticsFile::Many(many) => many,
  })
}
#[cfg(test)]
mod tests {
  use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
  };

  use super::*;
  use effect_model::{EffectFlags, EffectSummary, ThrowBehavior};

  #[test]
  fn parse_yaml_file_single_and_list() {
    let one = r#"
name: Array.prototype.map
effects: Pure
purity: DependsOnCallback
"#;
    let parsed = parse_api_semantics_yaml_str(one).unwrap();
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].name, "Array.prototype.map");

    let many = r#"
- name: fs.readFileSync
  effects: Io
  purity: Impure
- name: maybe_throw
  effects:
    Custom:
      flags: ALLOCATES
      throws: Maybe
  purity: Pure
"#;
    let parsed = parse_api_semantics_yaml_str(many).unwrap();
    assert_eq!(parsed.len(), 2);
    assert_eq!(parsed[0].effects, EffectTemplate::Io);
    assert_eq!(parsed[0].purity, PurityTemplate::Impure);

    assert_eq!(
      parsed[1].effects,
      EffectTemplate::Custom(EffectSummary {
        flags: EffectFlags::ALLOCATES,
        throws: ThrowBehavior::Maybe,
      })
    );
  }

  #[test]
  fn database_indexes_by_name() {
    let db = ApiDatabase::from_entries([ApiSemantics {
      name: "x".to_string(),
      aliases: vec![],
      effects: EffectTemplate::Pure,
      purity: PurityTemplate::Pure,
      async_: None,
      idempotent: None,
      deterministic: None,
      parallelizable: None,
      properties: BTreeMap::new(),
    }]);

    assert_eq!(db.get("x").unwrap().purity, PurityTemplate::Pure);
  }

  #[test]
  fn node_and_web_modules_parse_and_have_unique_api_names() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut yaml_paths = Vec::new();
    collect_yaml_files(&root.join("web"), &mut yaml_paths);
    yaml_paths.sort();

    let mut seen = BTreeSet::new();

    for path in yaml_paths {
      let src = fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!("failed to read {}: {err}", path.display());
      });
      let entries = parse_api_semantics_yaml_str(&src).unwrap_or_else(|err| {
        panic!("failed to parse {}: {err}", path.display());
      });
      for entry in entries {
        assert!(
          seen.insert(entry.name.clone()),
          "duplicate API `{}` (while loading {})",
          entry.name,
          path.display(),
        );
      }
    }
  }

  #[test]
  fn load_default_includes_metadata_flags_and_roundtrips() {
    let kb = KnowledgeBase::load_default().expect("load bundled knowledge base");

    let fetch = kb.get("fetch").expect("fetch in knowledge base");
    assert_eq!(fetch.async_, Some(true));
    assert_eq!(fetch.parallelizable, Some(true));
    assert_eq!(fetch.idempotent, Some(false));
    assert_eq!(fetch.deterministic, Some(false));

    let sqrt = kb.get("Math.sqrt").expect("Math.sqrt in knowledge base");
    assert_eq!(sqrt.deterministic, Some(true));
    assert_eq!(sqrt.idempotent, Some(true));
    assert_eq!(sqrt.async_, Some(false));

    let date_now = kb.get("Date.now").expect("Date.now in knowledge base");
    assert_eq!(date_now.async_, Some(false));
    assert_eq!(date_now.deterministic, Some(false));
    assert_eq!(date_now.idempotent, Some(false));

    let yaml = serde_yaml::to_string(fetch).unwrap();
    let parsed: ApiSemantics = serde_yaml::from_str(&yaml).unwrap();
    assert_eq!(&parsed, fetch);
  }

  #[test]
  fn encoding_contracts_minimum_set() {
    let kb = KnowledgeBase::load_default().expect("load bundled knowledge base");

    let slice = kb.get("String.prototype.slice").unwrap();
    assert_eq!(
      slice.properties.get("encoding.output").map(|s| s.as_str()),
      Some("same_as_input")
    );

    let concat = kb.get("String.prototype.concat").unwrap();
    assert_eq!(
      concat.properties.get("encoding.output").map(|s| s.as_str()),
      Some("unknown")
    );

    let lower = kb.get("String.prototype.toLowerCase").unwrap();
    assert_eq!(
      lower.properties.get("encoding.output").map(|s| s.as_str()),
      Some("same_as_input")
    );
    assert_eq!(
      lower
        .properties
        .get("encoding.preserves_input_if")
        .map(|s| s.as_str()),
      Some("ascii")
    );
    assert_eq!(
      lower
        .properties
        .get("encoding.length_preserving_if")
        .map(|s| s.as_str()),
      Some("ascii")
    );

    let split = kb.get("String.prototype.split").unwrap();
    assert_eq!(
      split.properties.get("encoding.output").map(|s| s.as_str()),
      Some("unknown")
    );

    let iso = kb.get("Date.prototype.toISOString").unwrap();
    assert_eq!(
      iso.properties.get("encoding.output").map(|s| s.as_str()),
      Some("ascii")
    );

    let pathname = kb.get("URL.prototype.pathname").unwrap();
    assert_eq!(
      pathname.properties.get("encoding.output").map(|s| s.as_str()),
      Some("ascii")
    );
  }

  fn collect_yaml_files(dir: &Path, out: &mut Vec<PathBuf>) {
    if !dir.exists() {
      return;
    }

    for entry in fs::read_dir(dir).unwrap() {
      let entry = entry.unwrap();
      let path = entry.path();
      if path.is_dir() {
        collect_yaml_files(&path, out);
        continue;
      }
      if path.extension().and_then(|s| s.to_str()) == Some("yaml") {
        out.push(path);
      }
    }
  }

  #[test]
  fn load_default_bundled_kb_includes_toml_and_validates() {
    let kb = KnowledgeBase::load_default().expect("load bundled knowledge base");
    kb.validate().expect("validate knowledge base");

    // This entry lives in `core/example.toml` and exercises the TOML loader.
    assert!(kb.get("Math.ceil").is_some());
  }
}
