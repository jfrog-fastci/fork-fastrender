use types_ts_interned::{
  GcTraceLayout, ObjectType, PropData, PropKey, Property, Shape, TupleElem, TypeKind, TypeStore,
};

fn tuple_elem(ty: types_ts_interned::TypeId) -> TupleElem {
  TupleElem {
    ty,
    optional: false,
    rest: false,
    readonly: false,
  }
}

#[test]
fn struct_with_mixed_scalar_and_gc_ptr_fields_is_flat() {
  let store = TypeStore::new();
  let prim = store.primitive_ids();

  let tuple = store.intern_type(TypeKind::Tuple(vec![
    tuple_elem(prim.boolean),
    tuple_elem(prim.string),
    tuple_elem(prim.number),
    tuple_elem(prim.string),
  ]));

  let layout = store.layout_of(tuple);
  let trace = store.gc_trace_layout(layout);

  assert_eq!(trace.as_flat_ptr_offsets(), Some(&[8, 24][..]));
  assert!(!trace.requires_tag_dispatch());
}

#[test]
fn nested_struct_offsets_reflect_recursion() {
  let store = TypeStore::new();
  let prim = store.primitive_ids();

  let inner = store.intern_type(TypeKind::Tuple(vec![
    tuple_elem(prim.boolean),
    tuple_elem(prim.string),
  ]));
  let outer = store.intern_type(TypeKind::Tuple(vec![
    tuple_elem(prim.number),
    tuple_elem(inner),
  ]));

  let layout = store.layout_of(outer);
  let trace = store.gc_trace_layout(layout);

  assert_eq!(trace.as_flat_ptr_offsets(), Some(&[16][..]));
  assert!(!trace.requires_tag_dispatch());
}

#[test]
fn union_with_conditional_pointers_is_tagged_union_trace() {
  let store = TypeStore::new();
  let prim = store.primitive_ids();

  let name_x = store.intern_name("x");
  let mut shape = Shape::new();
  shape.properties.push(Property {
    key: PropKey::String(name_x),
    data: PropData {
      ty: prim.string,
      optional: false,
      readonly: false,
      accessibility: None,
      is_method: false,
      origin: None,
      declared_on: None,
    },
  });
  let shape = store.intern_shape(shape);
  let obj = store.intern_object(ObjectType { shape });
  let obj_ty = store.intern_type(TypeKind::Object(obj));

  let union = store.intern_type(TypeKind::Union(vec![prim.number, obj_ty]));
  let layout_id = store.layout_of(union);

  let layout = store.layout(layout_id);
  let trace = store.gc_trace_layout(layout_id);
  assert!(trace.requires_tag_dispatch());
  assert!(trace.as_flat_ptr_offsets().is_none());

  let types_ts_interned::Layout::TaggedUnion {
    tag: layout_tag,
    payload_offset: layout_payload_offset,
    variants: layout_variants,
    ..
  } = layout
  else {
    panic!("expected union to lower to TaggedUnion layout");
  };

  let GcTraceLayout::TaggedUnion {
    tag: trace_tag,
    payload_offset: trace_payload_offset,
    variants: trace_variants,
  } = trace
  else {
    panic!("expected gc_trace() to return TaggedUnion");
  };

  assert_eq!(layout_tag, trace_tag);
  assert_eq!(layout_payload_offset, trace_payload_offset);
  assert_eq!(layout_variants.len(), trace_variants.len());

  for (layout_variant, trace_variant) in layout_variants.iter().zip(trace_variants.into_iter()) {
    assert_eq!(layout_variant.discriminant, trace_variant.discriminant);
    assert_eq!(layout_variant.payload_offset, trace_variant.payload_offset);
    assert_eq!(
      store.gc_trace_layout(layout_variant.layout),
      trace_variant.trace
    );
  }
}

#[test]
fn scalar_union_is_pointer_free() {
  let store = TypeStore::new();
  let prim = store.primitive_ids();

  let union = store.intern_type(TypeKind::Union(vec![prim.boolean, prim.number]));
  let layout_id = store.layout_of(union);
  let trace = store.gc_trace_layout(layout_id);

  assert!(matches!(trace, GcTraceLayout::None));
  assert_eq!(trace.as_flat_ptr_offsets(), Some(&[][..]));
  assert!(!trace.requires_tag_dispatch());
}

#[test]
fn gc_trace_is_deterministic_across_stores() {
  fn build() -> (std::sync::Arc<TypeStore>, types_ts_interned::LayoutId) {
    let store = TypeStore::new();
    let prim = store.primitive_ids();

    let name_x = store.intern_name("x");
    let mut shape = Shape::new();
    shape.properties.push(Property {
      key: PropKey::String(name_x),
      data: PropData {
        ty: prim.string,
        optional: false,
        readonly: false,
        accessibility: None,
        is_method: false,
        origin: None,
        declared_on: None,
      },
    });
    let shape = store.intern_shape(shape);
    let obj = store.intern_object(ObjectType { shape });
    let obj_ty = store.intern_type(TypeKind::Object(obj));

    let union = store.intern_type(TypeKind::Union(vec![prim.number, obj_ty]));
    let layout_id = store.layout_of(union);
    (store, layout_id)
  }

  let (store_a, layout_a) = build();
  let (store_b, layout_b) = build();
  assert_eq!(layout_a, layout_b);

  let trace_a = store_a.gc_trace_layout(layout_a);
  let trace_b = store_b.gc_trace_layout(layout_b);
  assert_eq!(trace_a, trace_b);
}
