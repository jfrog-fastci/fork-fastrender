use hir_js::{Body, BodyId, LowerResult};
use knowledge_base::ApiDatabase;

use crate::semantic_patterns::PatternTables;
use crate::types::TypeProvider;

/// Canonical pattern recognition façade for downstream crates.
///
/// This module intentionally exposes the table-based [`PatternTables`] output so
/// callers can:
/// - query per-expression resolved API IDs (`resolved_call`),
/// - query patterns rooted at each expression (`patterns`), and
/// - inspect the flat list of recognized patterns (`recognized`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatternEngineResult {
  pub tables: PatternTables,
}

pub fn analyze_patterns(
  lowered: &LowerResult,
  body_id: BodyId,
  body: &Body,
  db: &ApiDatabase,
  types: Option<&dyn TypeProvider>,
) -> PatternEngineResult {
  PatternEngineResult {
    tables: crate::semantic_patterns::recognize_pattern_tables(lowered, body_id, body, db, types),
  }
}

