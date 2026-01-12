use std::sync::{Arc, Barrier};
use std::thread;

use types_ts_interned::{
  CacheConfig, EvaluatorCaches, IntrinsicKind, RelateCtx, RelationCache, TypeEvaluator,
  TypeExpander, TypeId, TypeKind, TypeStore,
};

#[derive(Debug)]
struct NoopExpander;

impl TypeExpander for NoopExpander {
  fn expand(
    &self,
    _store: &TypeStore,
    _def: types_ts_interned::DefId,
    _args: &[TypeId],
  ) -> Option<types_ts_interned::ExpandedType> {
    None
  }
}

fn build_expensive_enough_type(store: &Arc<TypeStore>) -> (TypeId, TypeId) {
  let a = store.intern_name("a");
  let upper = store.intern_name("A");
  let input = store.intern_type(TypeKind::Intrinsic {
    kind: IntrinsicKind::Uppercase,
    ty: store.intern_type(TypeKind::StringLiteral(a)),
  });
  let expected = store.intern_type(TypeKind::StringLiteral(upper));
  (input, expected)
}

#[test]
fn evaluator_step_limit_is_deterministic_under_shared_cache_warmup() {
  let store = TypeStore::new();
  let (input, expected_warm) = build_expensive_enough_type(&store);

  let caches = EvaluatorCaches::new(CacheConfig {
    max_entries: 64,
    shard_count: 1,
  });

  let cold = {
    let expander = NoopExpander;
    let mut evaluator = TypeEvaluator::with_caches(store.clone(), &expander, caches.clone())
      .with_step_limit(0);
    evaluator.evaluate(input)
  };

  let warm = {
    let expander = NoopExpander;
    let mut evaluator = TypeEvaluator::with_caches(store.clone(), &expander, caches.clone());
    evaluator.evaluate(input)
  };
  assert_eq!(warm, expected_warm, "sanity check: warm evaluation must complete");

  let warmed_caches_limited = {
    let expander = NoopExpander;
    let mut evaluator = TypeEvaluator::with_caches(store.clone(), &expander, caches.clone())
      .with_step_limit(0);
    evaluator.evaluate(input)
  };

  assert_eq!(
    warmed_caches_limited, cold,
    "step-limited evaluation must not depend on whether another call warmed caches"
  );
}

#[test]
fn evaluator_step_limit_is_deterministic_under_parallel_cache_warmup() {
  let store = TypeStore::new();
  let (input, expected_warm) = build_expensive_enough_type(&store);

  let caches = EvaluatorCaches::new(CacheConfig {
    max_entries: 64,
    shard_count: 1,
  });

  let start = Arc::new(Barrier::new(2));
  let warmed = Arc::new(Barrier::new(2));

  let warm_thread = {
    let store = store.clone();
    let caches = caches.clone();
    let start = start.clone();
    let warmed = warmed.clone();
    thread::spawn(move || {
      let expander = NoopExpander;
      let mut evaluator = TypeEvaluator::with_caches(store, &expander, caches);
      start.wait();
      let res = evaluator.evaluate(input);
      warmed.wait();
      res
    })
  };

  let limited_thread = {
    let store = store.clone();
    let caches = caches.clone();
    let start = start.clone();
    let warmed = warmed.clone();
    thread::spawn(move || {
      let expander = NoopExpander;
      let mut evaluator =
        TypeEvaluator::with_caches(store, &expander, caches).with_step_limit(0);
      start.wait();
      warmed.wait();
      evaluator.evaluate(input)
    })
  };

  let warm = warm_thread.join().expect("warm thread panicked");
  assert_eq!(warm, expected_warm, "sanity check: warm evaluation must complete");

  let limited = limited_thread.join().expect("limited thread panicked");
  assert_eq!(
    limited, input,
    "step-limited evaluation must not observe cache warmup from another thread"
  );
}

#[test]
fn relation_step_limit_is_deterministic_under_shared_cache_warmup() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let cache = RelationCache::new(CacheConfig {
    max_entries: 64,
    shard_count: 1,
  });

  let cold = {
    let ctx =
      RelateCtx::with_cache(store.clone(), store.options(), cache.clone()).with_step_limit(0);
    ctx.is_assignable(primitives.number, primitives.string)
  };
  assert!(
    cold,
    "sanity check: a 0-step limit must conservatively assume assignability"
  );

  let warm = {
    let ctx = RelateCtx::with_cache(store.clone(), store.options(), cache.clone());
    ctx.is_assignable(primitives.number, primitives.string)
  };
  assert!(
    !warm,
    "sanity check: with no step limit, number should not be assignable to string"
  );

  let warmed_caches_limited = {
    let ctx =
      RelateCtx::with_cache(store.clone(), store.options(), cache.clone()).with_step_limit(0);
    ctx.is_assignable(primitives.number, primitives.string)
  };
  assert_eq!(
    warmed_caches_limited, cold,
    "step-limited relation must not depend on whether another call warmed caches"
  );
}
