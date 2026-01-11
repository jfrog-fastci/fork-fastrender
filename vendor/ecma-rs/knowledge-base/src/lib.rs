mod target;
mod version_range;

pub use target::{TargetEnv, WebPlatform};
pub use version_range::{VersionRange, VersionRangeSpec};

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use effect_model::{EffectSet, EffectTemplate, Purity, PurityTemplate, ThrowBehavior};
use semver::Version;
use serde::{de::Error as _, Deserialize, Serialize};
pub use serde_json::Value as JsonValue;

mod generated {
  include!(concat!(env!("OUT_DIR"), "/knowledge_base_generated.rs"));
}

mod ids;
pub use ids::ApiId;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ApiKind {
  Function,
  Constructor,
  #[serde(alias = "property", alias = "property_get")]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ApiEnv {
  Node,
  Web,
  Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ApiEntry {
  api: ApiSemantics,
  env: ApiEnv,
  platform: WebPlatform,
  source: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApiDatabase {
  apis: BTreeMap<String, Vec<ApiEntry>>,
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
    let mut apis = BTreeMap::<String, Vec<ApiEntry>>::new();
    for mut api in entries {
      api.id = ApiId::from_name(&api.name);
      apis.entry(api.name.clone()).or_default().push(ApiEntry {
        api,
        env: ApiEnv::Unknown,
        platform: WebPlatform::Generic,
        source: None,
      });
    }
    let sources = BTreeMap::new();
    let aliases = build_alias_map(&apis, &sources).unwrap_or_default();
    let ids = build_id_map(&apis, &sources).unwrap_or_default();
    Self {
      apis,
      aliases,
      ids,
      sources,
    }
  }

  pub fn get(&self, name_or_alias: &str) -> Option<&ApiSemantics> {
    self.api_for_target(name_or_alias, &TargetEnv::Unknown)
  }

  pub fn canonical_name(&self, name_or_alias: &str) -> Option<&str> {
    if let Some((key, _)) = self.apis.get_key_value(name_or_alias) {
      return Some(key.as_str());
    }
    self.aliases.get(name_or_alias).map(|s| s.as_str())
  }

  pub fn get_by_id(&self, id: ApiId) -> Option<&ApiSemantics> {
    let name = self.ids.get(&id)?;
    self.api_for_target(name, &TargetEnv::Unknown)
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
    self
      .apis
      .get(canonical)
      .and_then(|entries| entries.first())
      .map(|entry| entry.api.id)
  }

  pub fn iter(&self) -> impl Iterator<Item = (&str, &ApiSemantics)> {
    self.apis.iter().filter_map(|(name, entries)| {
      let mut best: Option<&ApiEntry> = None;
      for entry in entries {
        best = match best {
          None => Some(entry),
          Some(current) => Some(select_better(current, entry, &TargetEnv::Unknown)),
        };
      }
      best.map(|entry| (name.as_str(), &entry.api))
    })
  }

  /// Returns the best matching API entry for a given target environment.
  pub fn api_for_target(&self, name_or_alias: &str, target: &TargetEnv) -> Option<&ApiSemantics> {
    let canonical = self.canonical_name(name_or_alias)?;
    let entries = self.apis.get(canonical)?;

    let mut best: Option<&ApiEntry> = None;
    for entry in entries {
      if !entry_matches_target(entry, target) {
        continue;
      }
      best = match best {
        None => Some(entry),
        Some(current) => Some(select_better(current, entry, target)),
      };
    }

    best.map(|entry| &entry.api)
  }

  /// Load the bundled knowledge base embedded into the crate.
  ///
  /// This is a convenience alias for [`ApiDatabase::load_default`].
  pub fn from_embedded() -> Result<Self, KnowledgeBaseError> {
    Self::load_default()
  }

  pub fn load_default() -> Result<Self, KnowledgeBaseError> {
    Self::load_from_sources(generated::KB_FILES)
  }

  /// Load a knowledge base from explicit sources.
  ///
  /// Each entry is `(relative_path, file_contents)`. The `relative_path` is only
  /// used for diagnostics and format detection.
  pub fn load_from_sources(files: &[(&str, &str)]) -> Result<Self, KnowledgeBaseError> {
    let mut apis = BTreeMap::<String, Vec<ApiEntry>>::new();
    let mut sources = BTreeMap::<String, String>::new();

    for (path, contents) in files {
      let parsed = parse_source_file(path, contents)?;
      let (env, platform) = env_and_platform_for_path(path);
      let path_string = (*path).to_string();
      for api in parsed {
        // Duplicates are allowed as long as they have non-overlapping version ranges; keep the
        // first source path for stable diagnostics (individual entries retain their own sources).
        sources
          .entry(api.name.clone())
          .or_insert_with(|| path_string.clone());
        apis.entry(api.name.clone()).or_default().push(ApiEntry {
          api,
          env,
          platform,
          source: Some(path_string.clone()),
        });
      }
    }

    let aliases = build_alias_map(&apis, &sources)?;
    let ids = build_id_map(&apis, &sources)?;

    Ok(Self {
      apis,
      aliases,
      ids,
      sources,
    })
  }

  /// Load knowledge base modules directly from an on-disk `knowledge-base/` directory.
  ///
  /// The loader scans `core/`, `node/`, `web/`, and `ecosystem/` under `root`, and accepts
  /// `.yaml`/`.yml`/`.toml` files.
  pub fn load_from_dir(root: &Path) -> Result<Self, KnowledgeBaseError> {
    let mut files = Vec::new();
    for top in ["core", "node", "web", "ecosystem"] {
      let dir = root.join(top);
      if dir.exists() {
        collect_kb_files(&dir, root, &mut files)?;
      }
    }
    files.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));

    let mut apis = BTreeMap::<String, Vec<ApiEntry>>::new();
    let mut sources = BTreeMap::<String, String>::new();

    for file in files {
      let contents = fs::read_to_string(&file.abs_path).map_err(|err| KnowledgeBaseError::Io {
        path: file.abs_path.display().to_string(),
        source: err,
      })?;
      let parsed = parse_source_file(&file.rel_path, &contents)?;
      let (env, platform) = env_and_platform_for_path(&file.rel_path);
      for api in parsed {
        // Duplicates are allowed as long as they have non-overlapping version ranges; keep the
        // first source path for stable diagnostics (individual entries retain their own sources).
        sources
          .entry(api.name.clone())
          .or_insert_with(|| file.rel_path.clone());
        apis.entry(api.name.clone()).or_default().push(ApiEntry {
          api,
          env,
          platform,
          source: Some(file.rel_path.clone()),
        });
      }
    }

    let aliases = build_alias_map(&apis, &sources)?;
    let ids = build_id_map(&apis, &sources)?;

    Ok(Self {
      apis,
      aliases,
      ids,
      sources,
    })
  }

  pub fn validate(&self) -> Result<(), KnowledgeBaseError> {
    let aliases = build_alias_map(&self.apis, &self.sources)?;
    debug_assert_eq!(
      aliases, self.aliases,
      "ApiDatabase internal alias map is out of sync; please rebuild the database"
    );
    let ids = build_id_map(&self.apis, &self.sources)?;
    debug_assert_eq!(
      ids, self.ids,
      "ApiDatabase internal ApiId map is out of sync; please rebuild the database"
    );
    validate_versioned_duplicates(&self.apis)?;
    self.warn_inconsistent_metadata();
    Ok(())
  }

  fn warn_inconsistent_metadata(&self) {
    for entries in self.apis.values() {
      for entry in entries {
        let api = &entry.api;
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
}

fn env_and_platform_for_path(path: &str) -> (ApiEnv, WebPlatform) {
  let path = path.trim_start_matches("./");

  if let Some(rest) = path.strip_prefix("node/") {
    let _ = rest; // reserved for future per-module Node metadata
    return (ApiEnv::Node, WebPlatform::Generic);
  }

  if let Some(rest) = path.strip_prefix("web/") {
    let platform = if rest.starts_with("chrome/") {
      WebPlatform::Chrome
    } else if rest.starts_with("firefox/") {
      WebPlatform::Firefox
    } else if rest.starts_with("safari/") {
      WebPlatform::Safari
    } else {
      WebPlatform::Generic
    };
    return (ApiEnv::Web, platform);
  }

  (ApiEnv::Unknown, WebPlatform::Generic)
}

fn entry_version_range(entry: &ApiEntry) -> VersionRangeSpec {
  VersionRangeSpec::from_since_until(entry.api.since.as_deref(), entry.api.until.as_deref())
}

fn entry_matches_target(entry: &ApiEntry, target: &TargetEnv) -> bool {
  match target {
    TargetEnv::Unknown => true,
    TargetEnv::Node { version } => match entry.env {
      ApiEnv::Web => false,
      ApiEnv::Node | ApiEnv::Unknown => match entry_version_range(entry) {
        VersionRangeSpec::Parsed(range) => range.contains(version),
        VersionRangeSpec::Unparsed { .. } => false,
      },
    },
    TargetEnv::Web { platform } => {
      if matches!(entry.env, ApiEnv::Node) {
        return false;
      }
      // Conservative: if since/until aren't parseable, this entry is only usable under
      // `TargetEnv::Unknown`.
      if entry_version_range(entry).is_unparsed() {
        return false;
      }

      match entry.env {
        ApiEnv::Unknown => true,
        ApiEnv::Web => {
          if *platform == WebPlatform::Generic {
            entry.platform == WebPlatform::Generic
          } else {
            entry.platform == *platform || entry.platform == WebPlatform::Generic
          }
        }
        ApiEnv::Node => false,
      }
    }
  }
}

fn entry_since_version(entry: &ApiEntry) -> Option<Version> {
  match entry_version_range(entry) {
    VersionRangeSpec::Parsed(range) => range.since().cloned(),
    VersionRangeSpec::Unparsed { .. } => None,
  }
}

fn select_better<'a>(a: &'a ApiEntry, b: &'a ApiEntry, target: &TargetEnv) -> &'a ApiEntry {
  let a_spec = env_specificity(a, target);
  let b_spec = env_specificity(b, target);
  if a_spec != b_spec {
    return if b_spec > a_spec { b } else { a };
  }

  match (entry_since_version(a), entry_since_version(b)) {
    (Some(av), Some(bv)) if av != bv => return if bv > av { b } else { a },
    (None, Some(_)) => return b,
    (Some(_), None) => return a,
    _ => {}
  }

  // Deterministic final tie-breaker.
  let a_src = a.source.as_deref().unwrap_or("");
  let b_src = b.source.as_deref().unwrap_or("");
  if b_src < a_src { b } else { a }
}

fn env_specificity(entry: &ApiEntry, target: &TargetEnv) -> u8 {
  match target {
    TargetEnv::Node { .. } => match entry.env {
      ApiEnv::Node => 2,
      ApiEnv::Unknown => 1,
      ApiEnv::Web => 0,
    },
    TargetEnv::Web { platform } => match entry.env {
      ApiEnv::Web => {
        if *platform == entry.platform {
          3
        } else if entry.platform == WebPlatform::Generic {
          2
        } else {
          0
        }
      }
      ApiEnv::Unknown => 1,
      ApiEnv::Node => 0,
    },
    TargetEnv::Unknown => match entry.env {
      ApiEnv::Node => 2,
      ApiEnv::Web => 1,
      ApiEnv::Unknown => 0,
    },
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ApiEnvKey {
  Node,
  Web(WebPlatform),
  Unknown,
}

impl fmt::Display for ApiEnvKey {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      ApiEnvKey::Node => f.write_str("Node"),
      ApiEnvKey::Web(platform) => write!(f, "Web({platform:?})"),
      ApiEnvKey::Unknown => f.write_str("Unknown"),
    }
  }
}

fn env_key_for_entry(entry: &ApiEntry) -> ApiEnvKey {
  if entry_version_range(entry).is_unparsed() {
    return ApiEnvKey::Unknown;
  }

  match entry.env {
    ApiEnv::Node => ApiEnvKey::Node,
    ApiEnv::Web => ApiEnvKey::Web(entry.platform),
    ApiEnv::Unknown => ApiEnvKey::Unknown,
  }
}

fn ranges_overlap(a: &VersionRangeSpec, b: &VersionRangeSpec) -> bool {
  match (a.parsed(), b.parsed()) {
    (Some(ar), Some(br)) => ar.overlaps(br),
    // Conservative: if either side is unparseable, assume it overlaps.
    _ => true,
  }
}

fn validate_versioned_duplicates(
  apis: &BTreeMap<String, Vec<ApiEntry>>,
) -> Result<(), KnowledgeBaseError> {
  for (name, entries) in apis {
    let mut groups = HashMap::<ApiEnvKey, Vec<&ApiEntry>>::new();
    for entry in entries {
      groups.entry(env_key_for_entry(entry)).or_default().push(entry);
    }

    for (env_key, entries) in groups {
      for i in 0..entries.len() {
        for j in (i + 1)..entries.len() {
          let a = entries[i];
          let b = entries[j];
          let a_range = entry_version_range(a);
          let b_range = entry_version_range(b);
          if ranges_overlap(&a_range, &b_range) {
            return Err(KnowledgeBaseError::OverlappingApiRanges {
              name: name.clone(),
              env: env_key.to_string(),
              first: a.source.clone().unwrap_or_else(|| "<unknown>".to_string()),
              first_range: a_range.display(),
              second: b.source.clone().unwrap_or_else(|| "<unknown>".to_string()),
              second_range: b_range.display(),
            });
          }
        }
      }
    }
  }

  Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceFormat {
  Yaml,
  Toml,
}

impl fmt::Display for SourceFormat {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Yaml => f.write_str("YAML"),
      Self::Toml => f.write_str("TOML"),
    }
  }
}

#[derive(Debug, thiserror::Error)]
pub enum KnowledgeBaseError {
  #[error("failed to read knowledge base file `{path}`: {source}")]
  Io {
    path: String,
    #[source]
    source: std::io::Error,
  },

  #[error("failed to parse knowledge base file `{path}` as {format}: {source}")]
  Parse {
    path: String,
    format: SourceFormat,
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

  #[error(
    "duplicate alias `{alias}` for `{first_name}` ({first_source}) and `{second_name}` ({second_source})"
  )]
  DuplicateAlias {
    alias: String,
    first_name: String,
    first_source: String,
    second_name: String,
    second_source: String,
  },

  #[error(
    "overlapping API definitions for `{name}` in {env}: {first} ({first_range}) overlaps {second} ({second_range})"
  )]
  OverlappingApiRanges {
    name: String,
    env: String,
    first: String,
    first_range: String,
    second: String,
    second_range: String,
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

#[derive(Debug, Clone)]
enum EffectsRaw {
  Template(EffectTemplate),
  Details(EffectsDetailsRaw),
}

impl Default for EffectsRaw {
  fn default() -> Self {
    Self::Template(EffectTemplate::Unknown)
  }
}

impl<'de> Deserialize<'de> for EffectsRaw {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: serde::Deserializer<'de>,
  {
    let value = JsonValue::deserialize(deserializer)?;

    // Backwards compatibility: older KB entries (and some tests) used
    // `DependsOnCallback` / `depends_on_callback` as a scalar. The effect-model
    // now represents this as `DependsOnArgs { base, args }`.
    if let JsonValue::String(s) = &value {
      let token = s.trim().to_ascii_lowercase();
      if token == "dependsoncallback" || token == "depends_on_callback" {
        return Ok(Self::Template(EffectTemplate::DependsOnArgs {
          base: EffectSet::MAY_THROW,
          args: vec![0],
        }));
      }
    }

    // Prefer parsing as the canonical `EffectTemplate` (e.g. `Pure`, `Io`,
    // `Custom: { flags, throws }`) and fall back to the more permissive details
    // mapping (`{ template, allocates, io, ... }`).
    let template_res = serde_json::from_value::<EffectTemplate>(value.clone());
    if let Ok(template) = template_res {
      return Ok(Self::Template(template));
    }
    let details_res = serde_json::from_value::<EffectsDetailsRaw>(value);
    if let Ok(details) = details_res {
      return Ok(Self::Details(details));
    }

    Err(D::Error::custom(format!(
      "effects did not match EffectTemplate ({}) or EffectsDetailsRaw ({})",
      template_res
        .err()
        .expect("template parse failed but error was None"),
      details_res
        .err()
        .expect("details parse failed but error was None")
    )))
  }
}

#[derive(Debug, Clone, Deserialize, Default)]
struct EffectsDetailsRaw {
  // Legacy format: some KB files include a `base: [alloc, io, ...]` list for
  // human readability. These tokens are redundant with the boolean fields
  // below, but we keep parsing them for backwards compatibility.
  #[serde(default)]
  base: Vec<String>,

  // Legacy format: some KB files include `depends_on_args: [0, 1]` alongside
  // `template: depends_on_callback`. The modern schema encodes indices directly
  // in `EffectTemplate::DependsOnArgs`, so we only parse this field to avoid
  // rejecting older files.
  #[serde(default)]
  depends_on_args: Vec<usize>,
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

#[derive(Debug, Clone)]
enum PurityRaw {
  Template(PurityTemplate),
  Details(PurityDetailsRaw),
}

impl Default for PurityRaw {
  fn default() -> Self {
    Self::Template(PurityTemplate::Unknown)
  }
}

impl<'de> Deserialize<'de> for PurityRaw {
  fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
  where
    D: serde::Deserializer<'de>,
  {
    let value = JsonValue::deserialize(deserializer)?;

    // Backwards compatibility: older KB entries used `DependsOnCallback` /
    // `depends_on_callback` as a scalar. The effect-model now represents this
    // as `DependsOnArgs { base, args }`.
    if let JsonValue::String(s) = &value {
      let token = s.trim().to_ascii_lowercase();
      if token == "dependsoncallback" || token == "depends_on_callback" {
        return Ok(Self::Template(PurityTemplate::DependsOnArgs {
          base: Purity::Pure,
          args: vec![0],
        }));
      }
    }

    // Prefer parsing as a `PurityTemplate` (e.g. `Pure`, `ReadOnly`) and fall
    // back to the detail mapping (`{ template: ... }`).
    let template_res = serde_json::from_value::<PurityTemplate>(value.clone());
    if let Ok(template) = template_res {
      return Ok(Self::Template(template));
    }
    let details_res = serde_json::from_value::<PurityDetailsRaw>(value);
    if let Ok(details) = details_res {
      return Ok(Self::Details(details));
    }

    Err(D::Error::custom(format!(
      "purity did not match PurityTemplate ({}) or PurityDetailsRaw ({})",
      template_res
        .err()
        .expect("template parse failed but error was None"),
      details_res
        .err()
        .expect("details parse failed but error was None")
    )))
  }
}

#[derive(Debug, Clone, Deserialize, Default)]
struct PurityDetailsRaw {
  // Legacy format: some KB files include `kind: pure/read_only/...` in addition
  // to the template string. We ignore it but accept it for backwards
  // compatibility.
  #[serde(default)]
  kind: Option<String>,

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
    PurityRaw::Details(details) => match details.template.or(details.kind) {
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

      let mut base_allocates = false;
      let mut base_io = false;
      let mut base_network = false;
      let mut base_nondeterministic = false;
      let mut base_may_throw = false;
      let mut base_unknown = false;
      for token in &details.base {
        match normalize_ident(token).as_str() {
          "alloc" | "allocates" => base_allocates = true,
          "io" => base_io = true,
          "network" => base_network = true,
          "nondeterministic" | "non_deterministic" => base_nondeterministic = true,
          "may_throw" | "throws" => base_may_throw = true,
          "unknown" => base_unknown = true,
          // `async` is tracked by the top-level `async` API field, not the effect flags.
          _ => {}
        }
      }

      let unknown_default = template == "unknown";
      let io_default = template == "io";

      let mut flags = EffectSet::empty();
      if details
        .allocates
        .unwrap_or(unknown_default || base_allocates)
      {
        flags |= EffectSet::ALLOCATES;
      }
      if details.io.unwrap_or(io_default || unknown_default || base_io) {
        flags |= EffectSet::IO;
      }
      if details.network.unwrap_or(unknown_default || base_network) {
        flags |= EffectSet::NETWORK;
      }
      if details
        .nondeterministic
        .unwrap_or(unknown_default || base_nondeterministic)
      {
        flags |= EffectSet::NONDETERMINISTIC;
      }

      if unknown_default || base_unknown {
        flags |= EffectSet::UNKNOWN;
      }

      // Prefer the explicit `throws:` field (used by newer entries) over the
      // legacy `effects.may_throw` boolean.
      let may_throw = if let Some(throws) = throws.and_then(parse_throw_behavior) {
        !matches!(throws, ThrowBehavior::Never)
      } else if let Some(v) = details.may_throw {
        v
      } else {
        base_may_throw || template != "pure"
      };
      if may_throw {
        flags |= EffectSet::MAY_THROW;
      }

      let effect_template = if template == "depends_on_callback" {
        EffectTemplate::DependsOnArgs {
          base: flags,
          args: if details.depends_on_args.is_empty() {
            vec![0]
          } else {
            details.depends_on_args.clone()
          },
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

fn parse_source_file(path: &str, contents: &str) -> Result<Vec<ApiSemantics>, KnowledgeBaseError> {
  let ext = Path::new(path)
    .extension()
    .and_then(|s| s.to_str())
    .unwrap_or("")
    .to_ascii_lowercase();

  match ext.as_str() {
    "toml" => parse_toml_str(path, contents),
    // Default to YAML, even for unknown/missing extensions.
    _ => parse_yaml_str(path, contents),
  }
}

fn parse_yaml_str(path: &str, contents: &str) -> Result<Vec<ApiSemantics>, KnowledgeBaseError> {
  let value: serde_yaml::Value = serde_yaml::from_str(contents).map_err(|err| KnowledgeBaseError::Parse {
    path: path.to_string(),
    format: SourceFormat::Yaml,
    source: Box::new(err),
  })?;

  match value {
    serde_yaml::Value::Sequence(_) => {
      let apis: Vec<ApiRaw> = serde_yaml::from_value(value).map_err(|err| KnowledgeBaseError::Parse {
        path: path.to_string(),
        format: SourceFormat::Yaml,
        source: Box::new(err),
      })?;
      Ok(apis.into_iter().map(normalize_api).collect())
    }
    serde_yaml::Value::Mapping(map) => {
      let is_schema_module = map.contains_key(&serde_yaml::Value::String("schema".to_string()))
        || map.contains_key(&serde_yaml::Value::String("schema_version".to_string()));

      if is_schema_module {
        let module: ModuleRaw =
          serde_yaml::from_value(serde_yaml::Value::Mapping(map)).map_err(|err| KnowledgeBaseError::Parse {
            path: path.to_string(),
            format: SourceFormat::Yaml,
            source: Box::new(err),
          })?;
        if module.schema != 1 {
          return Err(KnowledgeBaseError::UnsupportedSchema {
            path: path.to_string(),
            schema: module.schema,
          });
        }
        Ok(module.apis.into_iter().map(normalize_api).collect())
      } else {
        let apis: BTreeMap<String, ApiBodyRaw> =
          serde_yaml::from_value(serde_yaml::Value::Mapping(map)).map_err(|err| KnowledgeBaseError::Parse {
            path: path.to_string(),
            format: SourceFormat::Yaml,
            source: Box::new(err),
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

fn parse_toml_str(path: &str, contents: &str) -> Result<Vec<ApiSemantics>, KnowledgeBaseError> {
  let module: ModuleRaw = toml::from_str(contents).map_err(|err| KnowledgeBaseError::Parse {
    path: path.to_string(),
    format: SourceFormat::Toml,
    source: Box::new(err),
  })?;
  if module.schema != 1 {
    return Err(KnowledgeBaseError::UnsupportedSchema {
      path: path.to_string(),
      schema: module.schema,
    });
  }

  Ok(module.apis.into_iter().map(normalize_api).collect())
}

fn build_id_map(
  apis: &BTreeMap<String, Vec<ApiEntry>>,
  sources: &BTreeMap<String, String>,
) -> Result<BTreeMap<ApiId, String>, KnowledgeBaseError> {
  let mut ids = BTreeMap::<ApiId, String>::new();

  for entries in apis.values() {
    for entry in entries {
      let api = &entry.api;
      if let Some(prev) = ids.get(&api.id).filter(|prev| *prev != &api.name) {
        let first_source = sources
          .get(prev)
          .cloned()
          .unwrap_or_else(|| "<unknown>".to_string());
        let second_source = sources
          .get(&api.name)
          .cloned()
          .or_else(|| entry.source.clone())
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
  }

  Ok(ids)
}

fn build_alias_map(
  apis: &BTreeMap<String, Vec<ApiEntry>>,
  sources: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, String>, KnowledgeBaseError> {
  let mut aliases = BTreeMap::<String, String>::new();

  for entries in apis.values() {
    for entry in entries {
      let api = &entry.api;
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

        if let Some(prev_entries) = apis.get(alias) {
          // Some modules materialize alias spellings as standalone entries (e.g. `fs.readFile`)
          // alongside a canonical form (e.g. `node:fs.readFile`) that also lists the alias.
          //
          // When the semantics match, this is redundant but unambiguous: lookups can resolve the
          // alias directly via the entry, so we skip building an alias mapping rather than treating
          // it as an error.
          if prev_entries
            .iter()
            .any(|prev| semantics_match(&prev.api, api))
          {
            continue;
          }

          return Err(KnowledgeBaseError::DuplicateAlias {
            alias: alias.to_string(),
            first_name: alias.to_string(),
            first_source: sources
              .get(alias)
              .cloned()
              .unwrap_or_else(|| "<unknown>".to_string()),
            second_name: api.name.clone(),
            second_source: sources
              .get(&api.name)
              .cloned()
              .or_else(|| entry.source.clone())
              .unwrap_or_else(|| "<unknown>".to_string()),
          });
        }

        match aliases.get(alias) {
          Some(prev) if prev == &api.name => continue,
          Some(prev) => {
            return Err(KnowledgeBaseError::DuplicateAlias {
              alias: alias.to_string(),
              first_name: prev.clone(),
              first_source: sources
                .get(prev)
                .cloned()
                .unwrap_or_else(|| "<unknown>".to_string()),
              second_name: api.name.clone(),
              second_source: sources
                .get(&api.name)
                .cloned()
                .or_else(|| entry.source.clone())
                .unwrap_or_else(|| "<unknown>".to_string()),
            })
          }
          None => {}
        }
        aliases.insert(alias.to_string(), api.name.clone());
      }
    }
  }

  Ok(aliases)
}

#[derive(Debug, Clone)]
struct DiskKbFile {
  rel_path: String,
  abs_path: PathBuf,
}

fn collect_kb_files(dir: &Path, root: &Path, out: &mut Vec<DiskKbFile>) -> Result<(), KnowledgeBaseError> {
  for entry in fs::read_dir(dir).map_err(|err| KnowledgeBaseError::Io {
    path: dir.display().to_string(),
    source: err,
  })? {
    let entry = entry.map_err(|err| KnowledgeBaseError::Io {
      path: dir.display().to_string(),
      source: err,
    })?;
    let path = entry.path();
    let ty = entry.file_type().map_err(|err| KnowledgeBaseError::Io {
      path: path.display().to_string(),
      source: err,
    })?;
    if ty.is_dir() {
      collect_kb_files(&path, root, out)?;
      continue;
    }
    if !ty.is_file() {
      continue;
    }

    let Some(ext) = path.extension().and_then(|ext| ext.to_str()) else {
      continue;
    };
    let ext = ext.to_ascii_lowercase();
    match ext.as_str() {
      "yaml" | "yml" | "toml" => {}
      _ => continue,
    }

    let rel_path = path
      .strip_prefix(root)
      .unwrap_or(&path)
      .to_string_lossy()
      .replace('\\', "/");

    out.push(DiskKbFile {
      rel_path,
      abs_path: path,
    });
  }
  Ok(())
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

  fn test_api(
    name: &str,
    since: Option<&str>,
    until: Option<&str>,
    semantics: Option<&str>,
  ) -> ApiSemantics {
    ApiSemantics {
      id: ApiId::from_name(name),
      name: name.to_string(),
      aliases: vec![],
      effects: EffectTemplate::Pure,
      effect_summary: EffectSet::empty(),
      purity: PurityTemplate::Pure,
      async_: None,
      idempotent: None,
      deterministic: None,
      parallelizable: None,
      semantics: semantics.map(|s| s.to_string()),
      signature: None,
      since: since.map(|s| s.to_string()),
      until: until.map(|s| s.to_string()),
      kind: ApiKind::Function,
      properties: BTreeMap::new(),
    }
  }

  fn test_entry(api: ApiSemantics, env: ApiEnv, source: &str) -> ApiEntry {
    ApiEntry {
      api,
      env,
      platform: WebPlatform::Generic,
      source: Some(source.to_string()),
    }
  }

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
  fn api_id_from_raw_roundtrips() {
    let id = ApiId::from_name("JSON.parse");
    assert_eq!(ApiId::from_raw(id.raw()), id);
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
  fn web_modules_parse_via_parse_api_semantics_yaml_str() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut yaml_paths = Vec::new();
    collect_yaml_files(&root.join("web"), &mut yaml_paths);
    yaml_paths.sort();

    let mut apis = BTreeMap::<String, Vec<ApiEntry>>::new();
    for path in yaml_paths {
      let src = fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!("failed to read {}: {err}", path.display());
      });
      let entries = parse_api_semantics_yaml_str(&src).unwrap_or_else(|err| {
        panic!("failed to parse {}: {err}", path.display());
      });
      let rel = path.strip_prefix(&root).unwrap_or(&path);
      let rel = rel.to_string_lossy().replace('\\', "/");
      let (env, platform) = env_and_platform_for_path(&rel);
      for api in entries {
        apis.entry(api.name.clone()).or_default().push(ApiEntry {
          api,
          env,
          platform,
          source: Some(rel.clone()),
        });
      }
    }

    validate_versioned_duplicates(&apis).expect("web module ranges should be non-overlapping");
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
  fn load_from_dir_matches_bundled_default() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let disk = KnowledgeBase::load_from_dir(&root).expect("load knowledge base from dir");
    disk.validate().expect("validate dir-loaded knowledge base");

    let bundled = KnowledgeBase::load_default().expect("load bundled knowledge base");
    bundled.validate().expect("validate bundled knowledge base");

    let disk_names: Vec<_> = disk.iter().map(|(name, _)| name.to_string()).collect();
    let bundled_names: Vec<_> = bundled.iter().map(|(name, _)| name.to_string()).collect();
    assert_eq!(disk_names, bundled_names);
  }

  #[test]
  fn bundled_kb_has_no_legacy_depends_on_callback_templates() {
    for (path, contents) in super::generated::KB_FILES {
      assert!(
        !contents.contains("DependsOnCallback"),
        "legacy template `DependsOnCallback` found in bundled knowledge base file `{path}`"
      );
    }
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
  fn core_array_yaml_parses_and_has_pipeline_metadata() {
    let yaml = include_str!("../core/array.yaml");
    let entries = parse_api_semantics_yaml_str(yaml).expect("parse core/array.yaml");
    let db = ApiDatabase::from_entries(entries);

    let map = db
      .get("Array.prototype.map")
      .expect("Array.prototype.map exists in core/array.yaml");
    let fusion_fusable_with = map
      .properties
      .get("fusion")
      .and_then(|v| v.as_object())
      .and_then(|obj| obj.get("fusable_with"))
      .and_then(|v| v.as_array())
      .expect("map.properties.fusion.fusable_with array");
    assert!(
      fusion_fusable_with
        .iter()
        .any(|v| v.as_str() == Some("Array.prototype.filter")),
      "expected map to declare fusable_with Array.prototype.filter"
    );

    let map_len_relation = map
      .properties
      .get("output")
      .and_then(|v| v.as_object())
      .and_then(|obj| obj.get("length_relation"))
      .and_then(|v| v.as_str());
    assert_eq!(map_len_relation, Some("same_as_input"));

    let parallel_requires_pure = map
      .properties
      .get("parallel")
      .and_then(|v| v.as_object())
      .and_then(|obj| obj.get("requires_callback_pure"))
      .and_then(|v| v.as_bool());
    assert_eq!(parallel_requires_pure, Some(true));
  }

  #[test]
  fn preserves_effect_summary_metadata() {
    let kb = KnowledgeBase::load_default().expect("load bundled knowledge base");

    let api = kb
      .get("node:fs.existsSync")
      .expect("node:fs.existsSync exists in bundled knowledge base");
    assert!(api.effect_summary.contains(EffectSet::IO));
    assert!(!api.effect_summary.contains(EffectSet::MAY_THROW));

    let map = kb
      .get("Array.prototype.map")
      .expect("Array.prototype.map exists in bundled knowledge base");
    assert!(map.effect_summary.contains(EffectSet::ALLOCATES));
  }

  #[test]
  fn overlapping_version_ranges_are_rejected() {
    let name = "node:test";
    let mut apis = BTreeMap::<String, Vec<ApiEntry>>::new();
    apis.entry(name.to_string()).or_default().push(test_entry(
      test_api(name, Some("18"), Some("20"), Some("v18")),
      ApiEnv::Node,
      "a.yaml",
    ));
    apis.entry(name.to_string()).or_default().push(test_entry(
      test_api(name, Some("19"), Some("21"), Some("v19")),
      ApiEnv::Node,
      "b.yaml",
    ));

    let sources = BTreeMap::new();
    let aliases = build_alias_map(&apis, &sources).unwrap();
    let ids = build_id_map(&apis, &sources).unwrap();
    let kb = ApiDatabase {
      apis,
      aliases,
      ids,
      sources,
    };

    let err = kb.validate().unwrap_err();
    match err {
      KnowledgeBaseError::OverlappingApiRanges {
        name: err_name,
        env,
        first,
        second,
        first_range,
        second_range,
      } => {
        assert_eq!(err_name, name);
        assert_eq!(env, "Node");
        assert_eq!(first, "a.yaml");
        assert_eq!(second, "b.yaml");
        assert!(!first_range.is_empty());
        assert!(!second_range.is_empty());
      }
      other => panic!("expected OverlappingApiRanges, got {other:?}"),
    }
  }

  #[test]
  fn non_overlapping_version_ranges_are_accepted() {
    let name = "node:test";
    let mut apis = BTreeMap::<String, Vec<ApiEntry>>::new();
    apis.entry(name.to_string()).or_default().push(test_entry(
      test_api(name, Some("18"), Some("20"), None),
      ApiEnv::Node,
      "a.yaml",
    ));
    apis.entry(name.to_string()).or_default().push(test_entry(
      test_api(name, Some("20"), Some("22"), None),
      ApiEnv::Node,
      "b.yaml",
    ));

    let sources = BTreeMap::new();
    let aliases = build_alias_map(&apis, &sources).unwrap();
    let ids = build_id_map(&apis, &sources).unwrap();
    let kb = ApiDatabase {
      apis,
      aliases,
      ids,
      sources,
    };

    kb.validate().expect("non-overlapping ranges should validate");
  }

  #[test]
  fn api_for_target_selects_correct_entry_for_node_version() {
    let name = "node:test";
    let mut apis = BTreeMap::<String, Vec<ApiEntry>>::new();
    apis.entry(name.to_string()).or_default().push(test_entry(
      test_api(name, None, None, Some("generic")),
      ApiEnv::Unknown,
      "generic.yaml",
    ));
    apis.entry(name.to_string()).or_default().push(test_entry(
      test_api(name, Some("18"), Some("20"), Some("v18")),
      ApiEnv::Node,
      "v18.yaml",
    ));
    apis.entry(name.to_string()).or_default().push(test_entry(
      test_api(name, Some("20"), None, Some("v20")),
      ApiEnv::Node,
      "v20.yaml",
    ));

    let sources = BTreeMap::new();
    let aliases = build_alias_map(&apis, &sources).unwrap();
    let ids = build_id_map(&apis, &sources).unwrap();
    let kb = ApiDatabase {
      apis,
      aliases,
      ids,
      sources,
    };

    let api_19 = kb
      .api_for_target(
        name,
        &TargetEnv::Node {
          version: Version::parse("19.0.0").unwrap(),
        },
      )
      .expect("v18 entry matches Node 19");
    assert_eq!(api_19.semantics.as_deref(), Some("v18"));

    let api_20 = kb
      .api_for_target(
        name,
        &TargetEnv::Node {
          version: Version::parse("20.0.0").unwrap(),
        },
      )
      .expect("v20 entry matches Node 20");
    assert_eq!(api_20.semantics.as_deref(), Some("v20"));
  }

  #[test]
  fn api_for_target_selects_web_entries_with_baseline_versions() {
    let kb = KnowledgeBase::load_default().expect("load bundled knowledge base");
    let fetch = kb
      .api_for_target(
        "fetch",
        &TargetEnv::Web {
          platform: WebPlatform::Generic,
        },
      )
      .expect("fetch should resolve for web targets");
    assert_eq!(fetch.name, "fetch");
  }

  #[test]
  fn env_and_platform_parses_web_subdirectories() {
    assert_eq!(
      env_and_platform_for_path("web/chrome/foo.yaml"),
      (ApiEnv::Web, WebPlatform::Chrome)
    );
    assert_eq!(
      env_and_platform_for_path("web/firefox/foo.yaml"),
      (ApiEnv::Web, WebPlatform::Firefox)
    );
    assert_eq!(
      env_and_platform_for_path("web/safari/foo.yaml"),
      (ApiEnv::Web, WebPlatform::Safari)
    );
    assert_eq!(
      env_and_platform_for_path("web/foo.yaml"),
      (ApiEnv::Web, WebPlatform::Generic)
    );
    assert_eq!(
      env_and_platform_for_path("node/foo.yaml"),
      (ApiEnv::Node, WebPlatform::Generic)
    );
  }
}
