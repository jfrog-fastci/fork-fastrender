use std::collections::BTreeMap;

use effect_model::{EffectTemplate, PurityTemplate};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApiSemantics {
  pub name: String,

  #[serde(default)]
  pub effects: EffectTemplate,

  #[serde(default)]
  pub purity: PurityTemplate,

  /// Arbitrary key/value metadata for downstream analyses.
  ///
  /// `effect-js` uses this for optional string encoding semantics such as:
  /// - `encoding.output: same_as_input`
  /// - `encoding.preserves_input_if: ascii`
  ///
  /// Values are strings to keep the schema stable and easy to author.
  #[serde(default)]
  pub properties: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApiDatabase {
  apis: BTreeMap<String, ApiSemantics>,
}

/// Backwards-compatible alias used by analysis passes.
pub type KnowledgeBase = ApiDatabase;

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

/// Schema v1 module file (the on-disk layout used under `knowledge-base/{node,web}`).
///
/// Format:
/// ```yaml
/// some.symbol:
///   aliases: [some.symbol]
///   since: "vX.Y.Z"
///   purity:
///     template: pure
///   throws: maybe
///   effects:
///     template: io
///     io: true
///     network: false
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct SchemaV1Module(pub BTreeMap<String, SchemaV1Entry>);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaV1Entry {
  #[serde(default)]
  pub aliases: Vec<String>,
  pub since: String,
  pub purity: SchemaV1Purity,
  pub throws: SchemaV1Throws,
  pub effects: SchemaV1Effects,

  #[serde(flatten)]
  pub extra: BTreeMap<String, serde_yaml::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaV1Purity {
  pub template: SchemaV1PurityTemplate,

  #[serde(flatten)]
  pub extra: BTreeMap<String, serde_yaml::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SchemaV1PurityTemplate {
  Pure,
  Readonly,
  Allocating,
  Impure,
  Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SchemaV1Throws {
  Never,
  Maybe,
  Always,
  Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaV1Effects {
  pub template: SchemaV1EffectTemplate,
  pub io: bool,
  pub network: bool,

  #[serde(flatten)]
  pub extra: BTreeMap<String, serde_yaml::Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SchemaV1EffectTemplate {
  Pure,
  Io,
  Unknown,
}

pub fn parse_schema_v1_yaml_str(yaml: &str) -> Result<SchemaV1Module, serde_yaml::Error> {
  serde_yaml::from_str(yaml)
}

#[cfg(test)]
mod tests {
  use std::{
    collections::BTreeSet,
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
      effects: EffectTemplate::Pure,
      purity: PurityTemplate::Pure,
      properties: BTreeMap::new(),
    }]);

    assert_eq!(db.get("x").unwrap().purity, PurityTemplate::Pure);
  }

  #[test]
  fn schema_v1_modules_parse_and_have_unique_symbol_names() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut yaml_paths = Vec::new();
    collect_yaml_files(&root.join("node"), &mut yaml_paths);
    collect_yaml_files(&root.join("web"), &mut yaml_paths);
    yaml_paths.sort();

    let mut seen = BTreeSet::new();

    for path in yaml_paths {
      let src = fs::read_to_string(&path).unwrap_or_else(|err| {
        panic!("failed to read {}: {err}", path.display());
      });
      let SchemaV1Module(entries) = parse_schema_v1_yaml_str(&src).unwrap_or_else(|err| {
        panic!("failed to parse {}: {err}", path.display());
      });
      for name in entries.keys() {
        assert!(
          seen.insert(name.clone()),
          "duplicate symbol `{name}` (while loading {})",
          path.display(),
        );
      }
    }
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
}
