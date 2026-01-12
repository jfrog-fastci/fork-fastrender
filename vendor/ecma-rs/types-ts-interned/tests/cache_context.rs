use types_ts_interned::{
  CacheConfig, EvaluatorCaches, RelationCache, RelateCtx, TypeEvaluator, TypeExpander, TypeOptions,
  TypeStore,
};

struct NoopExpander;

impl TypeExpander for NoopExpander {
  fn expand(
    &self,
    _store: &TypeStore,
    _def: types_ts_interned::DefId,
    _args: &[types_ts_interned::TypeId],
  ) -> Option<types_ts_interned::ExpandedType> {
    None
  }
}

#[test]
#[should_panic(expected = "EvaluatorCaches context hash mismatch")]
fn evaluator_cache_panics_when_reused_with_different_limits() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();
  let expander = NoopExpander;
  let caches = EvaluatorCaches::new(CacheConfig::default());

  let mut eval_small = TypeEvaluator::with_caches(store.clone(), &expander, caches.clone())
    .with_max_template_strings(4);
  let _ = eval_small.evaluate(primitives.string);

  let mut eval_large = TypeEvaluator::with_caches(store, &expander, caches)
    .with_max_template_strings(types_ts_interned::EvaluatorLimits::DEFAULT_MAX_TEMPLATE_STRINGS);
  let _ = eval_large.evaluate(primitives.string);
}

#[test]
#[should_panic(expected = "RelationCache context hash mismatch")]
fn relation_cache_panics_when_reused_with_different_options() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();
  let cache = RelationCache::default();

  let opts_a = TypeOptions::default();
  let ctx_a = RelateCtx::with_cache(store.clone(), opts_a, cache.clone());
  let _ = ctx_a.is_assignable(primitives.null, primitives.number);

  let mut opts_b = TypeOptions::default();
  opts_b.strict_null_checks = !opts_b.strict_null_checks;
  let ctx_b = RelateCtx::with_cache(store, opts_b, cache);
  let _ = ctx_b.is_assignable(primitives.null, primitives.number);
}
