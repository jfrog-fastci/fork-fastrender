use std::collections::BTreeMap;
use std::fmt;

use effect_model::{EffectSet, EffectTemplate, Purity, PurityTemplate, ThrowBehavior};
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

mod ids;
pub use ids::ApiId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApiKind {
  Function,
  Constructor,
  Getter,
  Setter,
  Value,
}

impl Default for ApiKind {
  fn default() -> Self {
    Self::Function
  }
}

impl ApiKind {
  fn is_function(&self) -> bool {
    matches!(self, Self::Function)
  }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApiSemantics {
  #[serde(skip)]
  pub id: ApiId,
  pub name: String,

  #[serde(default)]
  pub aliases: Vec<String>,

  #[serde(default)]
  pub effects: EffectTemplate,

  /// A non-template summary of the API's effects.
  ///
  /// This preserves author-provided base effect flags (allocates/io/etc) even
  /// when `effects` is a callback-dependent template.
  pub effect_summary: EffectSet,

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

  /// Free-form short semantics identifier (e.g. "Map", "Filter", "Debounce").
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub semantics: Option<String>,

  /// Optional signature hint for downstream tooling.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub signature: Option<String>,

  /// Version / availability metadata.
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub since: Option<String>,

  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub until: Option<String>,

  /// Function / constructor / getter / setter / value.
  #[serde(default, skip_serializing_if = "ApiKind::is_function")]
  pub kind: ApiKind,

  /// Arbitrary key/value metadata for downstream analyses.
  ///
  /// `effect-js` uses this for optional string encoding semantics such as:
  /// - `encoding.output: same_as_input`
  /// - `encoding.preserves_input_if: ascii`
  /// - `encoding.length_preserving_if: ascii`
  ///
  /// Values are JSON so the knowledge base can preserve structured metadata
  /// (booleans/numbers/arrays/maps) without losing author intent.
  #[serde(default)]
  pub properties: BTreeMap<String, JsonValue>,
}

#[derive(Debug, Deserialize)]
struct ApiSemanticsDeserialize {
  name: String,

  #[serde(default)]
  aliases: Vec<String>,

  #[serde(default)]
  effects: EffectTemplate,

  #[serde(default)]
  effect_summary: Option<EffectSet>,

  #[serde(default)]
  purity: PurityTemplate,

  #[serde(default, rename = "async")]
  async_: Option<bool>,

  #[serde(default)]
  idempotent: Option<bool>,

  #[serde(default)]
  deterministic: Option<bool>,

  #[serde(default)]
  parallelizable: Option<bool>,

  #[serde(default)]
  semantics: Option<String>,

  #[serde(default)]
  signature: Option<String>,

  #[serde(default)]
  since: Option<String>,

  #[serde(default)]
  until: Option<String>,

  #[serde(default)]
  kind: ApiKind,

  #[serde(default)]
  properties: BTreeMap<String, JsonValue>,
}

impl<'de> Deserialize<'de> for ApiSemantics {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: serde::Deserializer<'de>,
  {
    let raw = ApiSemanticsDeserialize::deserialize(deserializer)?;
    let effect_summary = raw
      .effect_summary
      .unwrap_or_else(|| effect_template_to_summary(&raw.effects));
    let id = ApiId::from_name(&raw.name);

    Ok(Self {
      id,
      name: raw.name,
      aliases: raw.aliases,
      effects: raw.effects,
      effect_summary,
      purity: raw.purity,
      async_: raw.async_,
      idempotent: raw.idempotent,
      deterministic: raw.deterministic,
      parallelizable: raw.parallelizable,
      semantics: raw.semantics,
      signature: raw.signature,
      since: raw.since,
      until: raw.until,
      kind: raw.kind,
      properties: raw.properties,
    })
  }
}

fn effect_template_to_summary(template: &EffectTemplate) -> EffectSet {
  match template {
    EffectTemplate::Pure => EffectSet::empty(),
    EffectTemplate::Io => EffectSet::IO | EffectSet::MAY_THROW,
    EffectTemplate::Custom(base) => *base,
    EffectTemplate::DependsOnArgs { base, .. } => *base,
    EffectTemplate::Unknown => EffectSet::UNKNOWN | EffectSet::MAY_THROW,
  }
}

impl ApiSemantics {
  pub fn effects_for_call(&self, arg_effects: &[EffectSet]) -> EffectSet {
    self.effects.apply(arg_effects)
  }

  pub fn purity_for_call(&self, arg_purity: &[Purity]) -> Purity {
    self.purity.apply(arg_purity)
  }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApiDatabase {
  apis: BTreeMap<String, ApiSemantics>,
  aliases: BTreeMap<String, String>,
  ids: BTreeMap<ApiId, String>,
  sources: BTreeMap<String, String>,
}

/// Backwards-compatible alias used by analysis passes.
pub type KnowledgeBase = ApiDatabase;

/// Backwards-compatible name used by `effect-js` query helpers.
pub type Api = ApiSemantics;

impl ApiDatabase {
  pub fn from_entries(entries: impl IntoIterator<Item = ApiSemantics>) -> Self {
    let mut apis = BTreeMap::new();
    for mut api in entries {
      api.id = ApiId::from_name(&api.name);
      apis.insert(api.name.clone(), api);
    }
    let sources = BTreeMap::new();
    let aliases = build_alias_map(&apis).unwrap_or_default();
    let ids = build_id_map(&apis, &sources).unwrap_or_default();
    Self {
      apis,
      aliases,
      ids,
      sources,
    }
  }

  pub fn get(&self, name_or_alias: &str) -> Option<&ApiSemantics> {
    let canonical = self.canonical_name(name_or_alias)?;
    self.apis.get(canonical)
  }

  pub fn canonical_name(&self, name_or_alias: &str) -> Option<&str> {
    if let Some((key, _)) = self.apis.get_key_value(name_or_alias) {
      return Some(key.as_str());
    }
    self.aliases.get(name_or_alias).map(|s| s.as_str())
  }

  pub fn get_by_id(&self, id: ApiId) -> Option<&ApiSemantics> {
    let name = self.ids.get(&id)?;
    self.apis.get(name)
  }

  /// Resolve `name_or_alias` into the canonical [`ApiId`].
  ///
  /// Aliases take precedence over direct name matches so redundant alias entries
  /// (e.g. `fs.readFile` alongside `node:fs.readFile`) resolve consistently.
  pub fn id_of(&self, name_or_alias: &str) -> Option<ApiId> {
    let canonical = self
      .aliases
      .get(name_or_alias)
      .map(|s| s.as_str())
      .unwrap_or(name_or_alias);
    self.apis.get(canonical).map(|api| api.id)
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

    let aliases = build_alias_map(&apis)?;
    let ids = build_id_map(&apis, &sources)?;

    Ok(Self {
      apis,
      aliases,
      ids,
      sources,
    })
  }

  pub fn validate(&self) -> Result<(), KnowledgeBaseError> {
    build_alias_map(&self.apis)?;
    build_id_map(&self.apis, &self.sources)?;
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

  #[error("API id collision 0x{id:x} between `{first_name}` ({first_source}) and `{second_name}` ({second_source})")]
  ApiIdCollision {
    id: u64,
    first_name: String,
    first_source: String,
    second_name: String,
    second_source: String,
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
  semantics: Option<String>,

  #[serde(default)]
  signature: Option<String>,

  #[serde(default)]
  since: Option<String>,

  #[serde(default)]
  until: Option<String>,

  #[serde(default)]
  kind: ApiKind,

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
  semantics: Option<String>,

  #[serde(default)]
  signature: Option<String>,

  #[serde(default)]
  since: Option<String>,

  #[serde(default)]
  until: Option<String>,

  #[serde(default)]
  kind: ApiKind,

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
  let (effects, effect_summary) = normalize_effects(raw.effects, raw.throws.as_deref());
  let purity = normalize_purity(raw.purity);
  let name = raw.name;
  ApiSemantics {
    id: ApiId::from_name(&name),
    name,
    aliases: raw.aliases,
    effects,
    effect_summary,
    purity,
    async_: raw.async_,
    idempotent: raw.idempotent,
    deterministic: raw.deterministic,
    parallelizable: raw.parallelizable,
    semantics: raw.semantics,
    signature: raw.signature,
    since: raw.since,
    until: raw.until,
    kind: raw.kind,
    properties: raw.properties,
  }
}

fn normalize_api_from_body(name: String, raw: ApiBodyRaw) -> ApiSemantics {
  let (effects, effect_summary) = normalize_effects(raw.effects, raw.throws.as_deref());
  let purity = normalize_purity(raw.purity);
  ApiSemantics {
    id: ApiId::from_name(&name),
    name,
    aliases: raw.aliases,
    effects,
    effect_summary,
    purity,
    async_: raw.async_,
    idempotent: raw.idempotent,
    deterministic: raw.deterministic,
    parallelizable: raw.parallelizable,
    semantics: raw.semantics,
    signature: raw.signature,
    since: raw.since,
    until: raw.until,
    kind: raw.kind,
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
    "depends_on_callback" => PurityTemplate::DependsOnArgs {
      base: Purity::Pure,
      args: vec![0],
    },
    "impure" => PurityTemplate::Impure,
    "unknown" => PurityTemplate::Unknown,
    _ => PurityTemplate::Unknown,
  }
}

fn normalize_effects(raw: EffectsRaw, throws: Option<&str>) -> (EffectTemplate, EffectSet) {
  match raw {
    EffectsRaw::Template(t) => {
      let summary = effect_template_to_summary(&t);
      (t, summary)
    }
    EffectsRaw::Details(details) => {
      let template = details
        .template
        .as_deref()
        .map(normalize_ident)
        .unwrap_or_default();

      let unknown_default = template == "unknown";
      let io_default = template == "io";

      let mut flags = EffectSet::empty();
      if details.allocates.unwrap_or(unknown_default) {
        flags |= EffectSet::ALLOCATES;
      }
      if details.io.unwrap_or(io_default || unknown_default) {
        flags |= EffectSet::IO;
      }
      if details.network.unwrap_or(unknown_default) {
        flags |= EffectSet::NETWORK;
      }
      if details.nondeterministic.unwrap_or(unknown_default) {
        flags |= EffectSet::NONDETERMINISTIC;
      }

      if unknown_default {
        flags |= EffectSet::UNKNOWN;
      }

      let may_throw = match details.may_throw {
        Some(v) => v,
        None => throws
          .and_then(parse_throw_behavior)
          .map(|b| !matches!(b, ThrowBehavior::Never))
          .unwrap_or_else(|| template != "pure"),
      };
      if may_throw {
        flags |= EffectSet::MAY_THROW;
      }

      let effect_template = if template == "depends_on_callback" {
        EffectTemplate::DependsOnArgs {
          base: flags,
          args: vec![0],
        }
      } else if flags.is_empty() {
        EffectTemplate::Pure
      } else if flags == (EffectSet::IO | EffectSet::MAY_THROW) {
        EffectTemplate::Io
      } else if flags == (EffectSet::UNKNOWN | EffectSet::MAY_THROW) {
        EffectTemplate::Unknown
      } else {
        EffectTemplate::Custom(flags)
      };

      (effect_template, flags)
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

fn build_id_map(
  apis: &BTreeMap<String, ApiSemantics>,
  sources: &BTreeMap<String, String>,
) -> Result<BTreeMap<ApiId, String>, KnowledgeBaseError> {
  let mut ids = BTreeMap::<ApiId, String>::new();

  for api in apis.values() {
    if let Some(prev) = ids.get(&api.id).filter(|prev| *prev != &api.name) {
      let first_source = sources
        .get(prev)
        .cloned()
        .unwrap_or_else(|| "<unknown>".to_string());
      let second_source = sources
        .get(&api.name)
        .cloned()
        .unwrap_or_else(|| "<unknown>".to_string());
      return Err(KnowledgeBaseError::ApiIdCollision {
        id: api.id.raw(),
        first_name: prev.clone(),
        first_source,
        second_name: api.name.clone(),
        second_source,
      });
    }

    ids.insert(api.id, api.name.clone());
  }

  Ok(ids)
}

fn build_alias_map(
  apis: &BTreeMap<String, ApiSemantics>,
) -> Result<BTreeMap<String, String>, KnowledgeBaseError> {
  let mut aliases = BTreeMap::<String, String>::new();

  for api in apis.values() {
    let node_alias = api.name.strip_prefix("node:");
    for alias in api
      .aliases
      .iter()
      .map(|s| s.as_str())
      .chain(node_alias)
    {
      if alias.is_empty() || alias == api.name {
        continue;
      }

      if let Some(prev) = apis.get(alias) {
        if semantics_match(prev, api) {
          continue;
        }

        return Err(KnowledgeBaseError::DuplicateAlias {
          alias: alias.to_string(),
          first: prev.name.clone(),
          second: api.name.clone(),
        });
      }

      match aliases.get(alias) {
        Some(prev) if prev == &api.name => continue,
        Some(prev) => {
          return Err(KnowledgeBaseError::DuplicateAlias {
            alias: alias.to_string(),
            first: prev.clone(),
            second: api.name.clone(),
          })
        }
        None => {}
      }
      aliases.insert(alias.to_string(), api.name.clone());
    }
  }

  Ok(aliases)
}

fn semantics_match(a: &ApiSemantics, b: &ApiSemantics) -> bool {
  a.effects == b.effects
    && a.effect_summary == b.effect_summary
    && a.purity == b.purity
    && a.async_ == b.async_
    && a.idempotent == b.idempotent
    && a.deterministic == b.deterministic
    && a.parallelizable == b.parallelizable
    && a.semantics == b.semantics
    && a.signature == b.signature
    && a.since == b.since
    && a.until == b.until
    && a.kind == b.kind
    && a.properties == b.properties
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
  // Fast path: the legacy "ApiSemantics list" format where `effects` and `purity` are directly
  // represented as `EffectTemplate` / `PurityTemplate` values.
  //
  // Some knowledge base files (notably Node/Web entries) use a more structured form for
  // `effects`/`purity` that needs normalization via `ApiRaw` (e.g. `effects.base`, `effects.io`,
  // `purity.template`). For those, fall back to parsing the YAML as a generic value and normalize.
  if let Ok(file) = serde_yaml::from_str::<ApiSemanticsFile>(yaml) {
    return Ok(match file {
      ApiSemanticsFile::One(one) => vec![one],
      ApiSemanticsFile::Many(many) => many,
    });
  }

  let value: serde_yaml::Value = serde_yaml::from_str(yaml)?;
  match value {
    serde_yaml::Value::Sequence(_) => {
      let apis: Vec<ApiRaw> = serde_yaml::from_value(value)?;
      Ok(apis.into_iter().map(normalize_api).collect())
    }
    serde_yaml::Value::Mapping(map) => {
      // Support a single entry written in the `ApiRaw` schema (e.g. with structured effects).
      if map.contains_key(&serde_yaml::Value::String("name".to_string())) {
        let api: ApiRaw = serde_yaml::from_value(serde_yaml::Value::Mapping(map))?;
        Ok(vec![normalize_api(api)])
      } else {
        Ok(Vec::new())
      }
    }
    _ => Ok(Vec::new()),
  }
}
#[cfg(test)]
mod tests {
  use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
  };

  use super::*;
  use effect_model::EffectSet;
  use serde_json::Value as JsonValue;

  #[test]
  fn parse_yaml_file_single_and_list() {
    let one = r#"
name: Array.prototype.map
effects: Pure
purity:
  depends_on_args:
    base: Allocating
    args: [0]
"#;
    let parsed = parse_api_semantics_yaml_str(one).unwrap();
    assert_eq!(parsed.len(), 1);
    assert_eq!(parsed[0].name, "Array.prototype.map");
    assert_eq!(parsed[0].kind, ApiKind::Function);

    let many = r#"
- name: fs.readFileSync
  effects: Io
  purity: Impure
- name: maybe_throw
  kind: getter
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
      EffectTemplate::Custom(EffectSet::ALLOCATES | EffectSet::MAY_THROW)
    );
    assert_eq!(parsed[1].kind, ApiKind::Getter);
  }

  #[test]
  fn database_indexes_by_name() {
    let db = ApiDatabase::from_entries([ApiSemantics {
      id: ApiId::from_name("x"),
      name: "x".to_string(),
      aliases: vec![],
      effects: EffectTemplate::Pure,
      effect_summary: EffectSet::empty(),
      purity: PurityTemplate::Pure,
      async_: None,
      idempotent: None,
      deterministic: None,
      parallelizable: None,
      semantics: None,
      signature: None,
      since: None,
      until: None,
      kind: ApiKind::Function,
      properties: BTreeMap::new(),
    }]);

    assert_eq!(db.get("x").unwrap().purity, PurityTemplate::Pure);
  }

  #[test]
  fn api_id_is_stable() {
    assert_eq!(ApiId::from_name("JSON.parse").raw(), 0xfb13ab6e4fa1910a);
  }

  #[test]
  fn load_default_has_unique_api_ids() {
    let kb = KnowledgeBase::load_default().expect("load bundled knowledge base");
    let mut ids = BTreeSet::new();
    for (_, api) in kb.iter() {
      assert!(ids.insert(api.id), "duplicate ApiId for {}", api.name);
    }
  }

  #[test]
  fn alias_lookup_resolves_to_canonical_id() {
    let kb = KnowledgeBase::load_default().expect("load bundled knowledge base");
    let alias = kb
      .id_of("fs.readFile")
      .expect("resolve alias name to ApiId");
    let canonical = kb
      .id_of("node:fs.readFile")
      .expect("resolve canonical name to ApiId");
    assert_eq!(alias, canonical);
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

    let url = kb.get("URL").unwrap();
    assert_eq!(url.kind, ApiKind::Constructor);

    let slice = kb.get("String.prototype.slice").unwrap();
    assert_eq!(
      slice
        .properties
        .get("encoding.output")
        .and_then(|v| v.as_str()),
      Some("same_as_input")
    );

    let concat = kb.get("String.prototype.concat").unwrap();
    assert_eq!(
      concat
        .properties
        .get("encoding.output")
        .and_then(|v| v.as_str()),
      Some("unknown")
    );

    let lower = kb.get("String.prototype.toLowerCase").unwrap();
    assert_eq!(
      lower
        .properties
        .get("encoding.output")
        .and_then(|v| v.as_str()),
      Some("same_as_input")
    );
    assert_eq!(
      lower
        .properties
        .get("encoding.preserves_input_if")
        .and_then(|v| v.as_str()),
      Some("ascii")
    );
    assert_eq!(
      lower
        .properties
        .get("encoding.length_preserving_if")
        .and_then(|v| v.as_str()),
      Some("ascii")
    );

    let trim = kb.get("String.prototype.trim").unwrap();
    assert_eq!(
      trim
        .properties
        .get("encoding.output")
        .and_then(|v| v.as_str()),
      Some("same_as_input")
    );

    let split = kb.get("String.prototype.split").unwrap();
    assert_eq!(
      split
        .properties
        .get("encoding.output")
        .and_then(|v| v.as_str()),
      Some("unknown")
    );

    let iso = kb.get("Date.prototype.toISOString").unwrap();
    assert_eq!(
      iso
        .properties
        .get("encoding.output")
        .and_then(|v| v.as_str()),
      Some("ascii")
    );

    let pathname = kb.get("URL.prototype.pathname").unwrap();
    assert_eq!(pathname.kind, ApiKind::Getter);
    assert_eq!(
      pathname
        .properties
        .get("encoding.output")
        .and_then(|v| v.as_str()),
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

  #[test]
  fn alias_lookup_resolves_node_prefix() {
    let kb = KnowledgeBase::load_default().expect("load bundled knowledge base");
    kb.validate().expect("validate knowledge base");

    let api = kb.get("fs.readFile").expect("fs.readFile resolves via alias");
    assert_eq!(api.name, "node:fs.readFile");
    assert_eq!(kb.canonical_name("fs.readFile"), Some("node:fs.readFile"));
  }

  #[test]
  fn preserves_ecosystem_properties() {
    let kb = KnowledgeBase::load_default().expect("load bundled knowledge base");

    let api = kb
      .get("lodash.debounce")
      .expect("lodash.debounce exists in bundled knowledge base");
    assert_eq!(api.properties.get("timer_based"), Some(&JsonValue::Bool(true)));
  }

  #[test]
  fn properties_support_non_string_values() {
    let yaml = r#"
name: x
effects: Pure
purity: Pure
properties:
  timer_based: true
  retry_delays: [10, 20]
  meta:
    level: 1
"#;

    let parsed = parse_api_semantics_yaml_str(yaml).expect("parse YAML");
    let api = parsed.first().expect("one entry");

    assert_eq!(api.properties.get("timer_based"), Some(&JsonValue::Bool(true)));

    let retry_delays = api
      .properties
      .get("retry_delays")
      .and_then(|v| v.as_array())
      .expect("retry_delays array");
    assert_eq!(retry_delays.len(), 2);
    assert_eq!(retry_delays[0].as_i64(), Some(10));
    assert_eq!(retry_delays[1].as_i64(), Some(20));

    let meta_level = api
      .properties
      .get("meta")
      .and_then(|v| v.as_object())
      .and_then(|obj| obj.get("level"))
      .and_then(|v| v.as_i64());
    assert_eq!(meta_level, Some(1));
  }

  #[test]
  fn preserves_effect_summary_metadata() {
    let kb = KnowledgeBase::load_default().expect("load bundled knowledge base");

    let api = kb
      .get("node:fs.existsSync")
      .expect("node:fs.existsSync exists in bundled knowledge base");
    assert!(api.effect_summary.contains(EffectSet::IO));
    assert!(!api.effect_summary.contains(EffectSet::MAY_THROW));
  }
}
