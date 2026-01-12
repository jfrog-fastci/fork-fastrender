use effect_js::{ApiDatabase, validate};

#[test]
fn knowledge_base_passes_lint() {
  let db = ApiDatabase::from_embedded().expect("knowledge base loads");
  db.validate().expect("knowledge base should pass core validation");
  validate::validate(&db).expect("knowledge base should pass effect-js lint");
}
