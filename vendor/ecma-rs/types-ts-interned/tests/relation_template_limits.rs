use types_ts_interned::{
  RelateCtx, RelationLimits, TemplateChunk, TemplateLiteralType, TypeKind, TypeStore,
};

#[test]
fn template_to_template_assignability_bails_on_long_head() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let src = store.intern_type(TypeKind::TemplateLiteral(TemplateLiteralType {
    head: "X".repeat(32),
    spans: vec![TemplateChunk {
      literal: "".into(),
      ty: primitives.string,
    }],
  }));

  // `${string}Y` (any string ending in `Y`), chosen to ensure the destination
  // remains a template literal type after normalization.
  let dst = store.intern_type(TypeKind::TemplateLiteral(TemplateLiteralType {
    head: "".into(),
    spans: vec![TemplateChunk {
      literal: "Y".into(),
      ty: primitives.string,
    }],
  }));

  let ctx = RelateCtx::new(store.clone(), store.options()).with_limits(RelationLimits {
    max_template_string_len: 8,
    max_template_total_bytes: usize::MAX,
    ..RelationLimits::default()
  });

  assert!(
    !ctx.is_assignable(src, dst),
    "expected long-head template enumeration to return None and conservatively fail"
  );
}

#[test]
fn template_enumeration_with_large_limits_preserves_assignability() {
  let store = TypeStore::new();

  let x = store.intern_type(TypeKind::StringLiteral(store.intern_name_ref("X")));

  let src = store.intern_type(TypeKind::TemplateLiteral(TemplateLiteralType {
    head: "X".into(),
    spans: Vec::new(),
  }));

  let dst = store.intern_type(TypeKind::TemplateLiteral(TemplateLiteralType {
    head: "".into(),
    spans: vec![TemplateChunk {
      literal: "".into(),
      ty: x,
    }],
  }));

  let ctx = RelateCtx::new(store.clone(), store.options()).with_limits(RelationLimits {
    max_template_string_len: usize::MAX,
    max_template_total_bytes: usize::MAX,
    ..RelationLimits::default()
  });

  assert!(ctx.is_assignable(src, dst));
}
