use knowledge_base::{parse_api_semantics_yaml_str, ApiDatabase};

const NODE_KB_FILES: &[(&str, &str)] = &[
  (
    "node/buffer.yaml",
    include_str!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/../knowledge-base/node/buffer.yaml"
    )),
  ),
  (
    "node/crypto.yaml",
    include_str!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/../knowledge-base/node/crypto.yaml"
    )),
  ),
  (
    "node/fs.yaml",
    include_str!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/../knowledge-base/node/fs.yaml"
    )),
  ),
  (
    "node/http.yaml",
    include_str!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/../knowledge-base/node/http.yaml"
    )),
  ),
  (
    "node/path.yaml",
    include_str!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/../knowledge-base/node/path.yaml"
    )),
  ),
  (
    "node/timers.yaml",
    include_str!(concat!(
      env!("CARGO_MANIFEST_DIR"),
      "/../knowledge-base/node/timers.yaml"
    )),
  ),
];

/// Load the repository's built-in API semantics database.
///
/// This is currently backed by YAML files under `knowledge-base/`.
pub fn load_default_api_database() -> ApiDatabase {
  let mut entries = Vec::new();
  for (path, yaml) in NODE_KB_FILES {
    let parsed = parse_api_semantics_yaml_str(yaml)
      .unwrap_or_else(|err| panic!("failed to parse knowledge base YAML {path}: {err}"));
    entries.extend(parsed);
  }

  ApiDatabase::from_entries(entries)
}
