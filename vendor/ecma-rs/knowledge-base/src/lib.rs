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
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApiDatabase {
  apis: BTreeMap<String, ApiSemantics>,
}

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

pub fn parse_api_semantics_yaml_str(yaml: &str) -> Result<Vec<ApiSemantics>, serde_yaml::Error> {
  let file: ApiSemanticsFile = serde_yaml::from_str(yaml)?;
  Ok(match file {
    ApiSemanticsFile::One(one) => vec![one],
    ApiSemanticsFile::Many(many) => many,
  })
}

#[cfg(test)]
mod tests {
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
    }]);

    assert_eq!(db.get("x").unwrap().purity, PurityTemplate::Pure);
  }
}

