use types_ts_interned::{
  DefId, EvaluatorLimits, ExpandedType, TemplateChunk, TemplateLiteralType, TypeEvaluator,
  TypeExpander, TypeId, TypeKind, TypeStore,
};

#[derive(Default)]
struct NoopExpander;

impl TypeExpander for NoopExpander {
  fn expand(&self, _store: &TypeStore, _def: DefId, _args: &[TypeId]) -> Option<ExpandedType> {
    None
  }
}

fn evaluator<'a>(store: std::sync::Arc<TypeStore>, expander: &'a NoopExpander) -> TypeEvaluator<'a, NoopExpander> {
  TypeEvaluator::new(store, expander)
}

fn build_long_template(store: &std::sync::Arc<TypeStore>, atom: TypeId) -> (TypeId, usize) {
  let long_len = 100_000;
  let head = "a".repeat(long_len);
  let tail = "b".repeat(long_len);
  let tpl = store.intern_type(TypeKind::TemplateLiteral(TemplateLiteralType {
    head,
    spans: vec![TemplateChunk {
      literal: tail,
      ty: atom,
    }],
  }));
  (tpl, long_len)
}

#[test]
fn template_total_bytes_limit_widens_to_string() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();
  let expander = NoopExpander;

  let atom = store.union(vec![
    store.intern_type(TypeKind::StringLiteral(store.intern_name_ref("x"))),
    store.intern_type(TypeKind::StringLiteral(store.intern_name_ref("y"))),
  ]);

  let (tpl, long_len) = build_long_template(&store, atom);

  // The concrete strings are ~200k bytes each; cap total bytes below that so the
  // evaluator must bail out to `string`.
  let mut eval = evaluator(store.clone(), &expander).with_limits(EvaluatorLimits {
    max_template_string_len: (long_len * 3),
    max_template_total_bytes: (long_len * 3) / 2,
    ..EvaluatorLimits::default()
  });
  assert_eq!(eval.evaluate(tpl), primitives.string);
}

#[test]
fn template_string_len_limit_widens_to_string() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();
  let expander = NoopExpander;

  let atom = store.union(vec![
    store.intern_type(TypeKind::StringLiteral(store.intern_name_ref("x"))),
    store.intern_type(TypeKind::StringLiteral(store.intern_name_ref("y"))),
  ]);

  let (tpl, _long_len) = build_long_template(&store, atom);

  let mut eval = evaluator(store.clone(), &expander).with_limits(EvaluatorLimits {
    max_template_string_len: 8 * 1024,
    max_template_total_bytes: 1024 * 1024,
    ..EvaluatorLimits::default()
  });
  assert_eq!(eval.evaluate(tpl), primitives.string);
}

#[test]
fn template_limits_allow_enumeration_when_large_enough() {
  let store = TypeStore::new();
  let expander = NoopExpander;

  let atom = store.union(vec![
    store.intern_type(TypeKind::StringLiteral(store.intern_name_ref("x"))),
    store.intern_type(TypeKind::StringLiteral(store.intern_name_ref("y"))),
  ]);

  let long_len = 100_000;
  let head = "a".repeat(long_len);
  let tail = "b".repeat(long_len);

  let expected_x = {
    let mut s = String::with_capacity(head.len() + 1 + tail.len());
    s.push_str(&head);
    s.push('x');
    s.push_str(&tail);
    s
  };
  let expected_y = {
    let mut s = String::with_capacity(head.len() + 1 + tail.len());
    s.push_str(&head);
    s.push('y');
    s.push_str(&tail);
    s
  };

  let tpl = store.intern_type(TypeKind::TemplateLiteral(TemplateLiteralType {
    head,
    spans: vec![TemplateChunk {
      literal: tail,
      ty: atom,
    }],
  }));

  let mut eval = evaluator(store.clone(), &expander).with_limits(EvaluatorLimits {
    max_template_string_len: 300_000,
    max_template_total_bytes: 1024 * 1024,
    ..EvaluatorLimits::default()
  });

  let expected = store.union(vec![
    store.intern_type(TypeKind::StringLiteral(store.intern_name(expected_x))),
    store.intern_type(TypeKind::StringLiteral(store.intern_name(expected_y))),
  ]);

  assert_eq!(eval.evaluate(tpl), expected);
}
