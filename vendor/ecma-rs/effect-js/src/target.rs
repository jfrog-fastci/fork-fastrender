use knowledge_base::{ApiDatabase, ApiId, ApiSemantics, TargetEnv};

/// A target-aware view over an [`ApiDatabase`].
///
/// The `knowledge-base` crate stores multiple per-environment definitions for some
/// APIs (e.g. Node vs Web). Since [`ApiId`] hashes only the canonical API name,
/// downstream analyses must provide a [`TargetEnv`] to consistently select the
/// correct per-target [`ApiSemantics`] entry.
#[derive(Debug, Clone)]
pub struct TargetedKb<'a> {
  db: &'a ApiDatabase,
  target: TargetEnv,
}

impl<'a> TargetedKb<'a> {
  pub fn new(db: &'a ApiDatabase, target: TargetEnv) -> Self {
    Self { db, target }
  }

  pub fn db(&self) -> &'a ApiDatabase {
    self.db
  }

  pub fn target(&self) -> &TargetEnv {
    &self.target
  }

  pub fn get(&self, name_or_alias: &str) -> Option<&'a ApiSemantics> {
    self.db.api_for_target(name_or_alias, &self.target)
  }

  pub fn api_for_target(&self, name_or_alias: &str) -> Option<&'a ApiSemantics> {
    self.db.api_for_target(name_or_alias, &self.target)
  }

  pub fn get_by_id(&self, id: ApiId) -> Option<&'a ApiSemantics> {
    self.db.get_by_id_for_target(id, &self.target)
  }

  pub fn canonical_name(&self, name_or_alias: &str) -> Option<&'a str> {
    self.db.canonical_name(name_or_alias)
  }

  pub fn id_of(&self, name_or_alias: &str) -> Option<ApiId> {
    self.db.id_of(name_or_alias)
  }
}

