use knowledge_base::ApiDatabase;

/// Load the repository's built-in API semantics database.
///
/// This uses the bundled KB files from the `knowledge-base` crate (YAML + TOML).
pub fn load_default_api_database() -> ApiDatabase {
  ApiDatabase::load_default().unwrap_or_else(|err| panic!("failed to load bundled knowledge base: {err}"))
}
