use effect_js::{ApiDatabase, validate};

#[test]
fn knowledge_base_passes_lint() {
  let db = ApiDatabase::from_embedded().expect("knowledge base loads");
  validate::validate(&db).expect("knowledge base should pass effect-js lint");
}

