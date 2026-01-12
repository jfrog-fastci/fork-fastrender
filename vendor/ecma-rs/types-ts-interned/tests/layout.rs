use std::collections::{BTreeMap, BTreeSet};

use types_ts_interned::{
  AbiScalar, FieldKey, Layout, LayoutId, ObjectType, PropData, PropKey, Property, Shape, TupleElem,
  TypeKind, TypeStore,
};

fn collect_layout_graph(store: &TypeStore, root: LayoutId) -> BTreeMap<LayoutId, Layout> {
  let mut visited: BTreeSet<LayoutId> = BTreeSet::new();
  let mut queue: Vec<LayoutId> = vec![root];
  let mut out = BTreeMap::new();

  while let Some(id) = queue.pop() {
    if !visited.insert(id) {
      continue;
    }

    let layout = store.layout(id);
    match &layout {
      Layout::Scalar { .. } => {}
      Layout::Ptr { to } => match to {
        types_ts_interned::PtrKind::GcObject { layout }
        | types_ts_interned::PtrKind::GcArray { elem: layout } => queue.push(*layout),
        types_ts_interned::PtrKind::GcString
        | types_ts_interned::PtrKind::GcAny
        | types_ts_interned::PtrKind::Opaque => {}
      },
      Layout::Struct { fields, .. } => {
        for field in fields {
          queue.push(field.layout);
        }
      }
      Layout::TaggedUnion { variants, .. } => {
        for variant in variants {
          queue.push(variant.layout);
        }
      }
    }
    out.insert(id, layout);
  }

  out
}

#[test]
fn determinism_across_stores() {
  fn build() -> (std::sync::Arc<TypeStore>, types_ts_interned::TypeId) {
    let store = TypeStore::new();
    let primitives = store.primitive_ids();

    let tuple = store.intern_type(TypeKind::Tuple(vec![
      TupleElem {
        ty: primitives.boolean,
        optional: false,
        rest: false,
        readonly: false,
      },
      TupleElem {
        ty: primitives.number,
        optional: false,
        rest: false,
        readonly: false,
      },
    ]));

    let name_a = store.intern_name("a");
    let name_b = store.intern_name("b");
    let mut shape = Shape::new();
    // Intentionally unsorted insertion order.
    shape.properties.push(Property {
      key: PropKey::String(name_b),
      data: PropData {
        ty: primitives.number,
        optional: false,
        readonly: false,
        accessibility: None,
        is_method: false,
        origin: None,
        declared_on: None,
      },
    });
    shape.properties.push(Property {
      key: PropKey::String(name_a),
      data: PropData {
        ty: primitives.boolean,
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

    let union = store.intern_type(TypeKind::Union(vec![tuple, obj_ty]));

    (store, union)
  }

  let (store_a, ty_a) = build();
  let (store_b, ty_b) = build();
  assert_eq!(ty_a, ty_b);

  let root_a = store_a.layout_of(ty_a);
  let root_b = store_b.layout_of(ty_b);
  assert_eq!(root_a, root_b);

  let graph_a = collect_layout_graph(store_a.as_ref(), root_a);
  let graph_b = collect_layout_graph(store_b.as_ref(), root_b);
  assert_eq!(graph_a, graph_b);
}

#[test]
fn tuple_layout_offsets() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let tuple = store.intern_type(TypeKind::Tuple(vec![
    TupleElem {
      ty: primitives.boolean,
      optional: false,
      rest: false,
      readonly: false,
    },
    TupleElem {
      ty: primitives.number,
      optional: false,
      rest: false,
      readonly: false,
    },
    TupleElem {
      ty: primitives.boolean,
      optional: false,
      rest: false,
      readonly: false,
    },
  ]));

  let id = store.layout_of(tuple);
  let Layout::Struct { fields, size, align } = store.layout(id) else {
    panic!("expected tuple to lower to Struct layout");
  };

  assert_eq!(align, 8);
  assert_eq!(size, 24);
  assert_eq!(fields.len(), 3);

  assert_eq!(fields[0].key, FieldKey::TupleIndex(0));
  assert_eq!(fields[0].offset, 0);
  assert_eq!(fields[0].size, 1);
  assert_eq!(fields[0].align, 1);

  assert_eq!(fields[1].key, FieldKey::TupleIndex(1));
  assert_eq!(fields[1].offset, 8);
  assert_eq!(fields[1].size, 8);
  assert_eq!(fields[1].align, 8);

  assert_eq!(fields[2].key, FieldKey::TupleIndex(2));
  assert_eq!(fields[2].offset, 16);
  assert_eq!(fields[2].size, 1);
  assert_eq!(fields[2].align, 1);
}

#[test]
fn union_variant_order_is_stable() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let union = store.intern_type(TypeKind::Union(vec![primitives.number, primitives.boolean]));
  let id = store.layout_of(union);

  let Layout::TaggedUnion { tag, variants, .. } = store.layout(id) else {
    panic!("expected union to lower to TaggedUnion layout");
  };

  assert_eq!(tag.abi, AbiScalar::U8);
  assert_eq!(variants.len(), 2);
  assert_eq!(variants[0].ty, primitives.boolean);
  assert_eq!(variants[1].ty, primitives.number);
}

#[test]
fn object_shape_field_ordering_is_stable() {
  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let name_a = store.intern_name("a");
  let name_b = store.intern_name("b");
  let mut shape = Shape::new();

  // Insert out-of-order; `intern_shape` canonicalizes by `PropKey::cmp_with`.
  shape.properties.push(Property {
    key: PropKey::String(name_b),
    data: PropData {
      ty: primitives.number,
      optional: false,
      readonly: false,
      accessibility: None,
      is_method: false,
      origin: None,
      declared_on: None,
    },
  });
  shape.properties.push(Property {
    key: PropKey::String(name_a),
    data: PropData {
      ty: primitives.boolean,
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

  let id = store.layout_of(obj_ty);
  let Layout::Ptr { to } = store.layout(id) else {
    panic!("expected object to lower to Ptr layout");
  };
  let types_ts_interned::PtrKind::GcObject { layout: payload } = to else {
    panic!("expected object PtrKind::GcObject");
  };

  let Layout::Struct { fields, .. } = store.layout(payload) else {
    panic!("expected object payload layout to be Struct");
  };

  assert_eq!(fields.len(), 2);
  assert_eq!(fields[0].key, FieldKey::Prop(PropKey::String(name_a)));
  assert_eq!(fields[1].key, FieldKey::Prop(PropKey::String(name_b)));
}

#[test]
fn callable_types_lower_to_traceable_closure_objects() {
  use types_ts_interned::{Param, Signature};

  let store = TypeStore::new();
  let primitives = store.primitive_ids();

  let sig = store.intern_signature(Signature::new(
    vec![Param {
      name: None,
      ty: primitives.number,
      optional: false,
      rest: false,
    }],
    primitives.boolean,
  ));
  let callable = store.intern_type(TypeKind::Callable {
    overloads: vec![sig],
  });

  let layout_id = store.layout_of(callable);
  let Layout::Ptr { to } = store.layout(layout_id) else {
    panic!("expected callable to lower to Ptr layout");
  };
  let types_ts_interned::PtrKind::GcObject { layout: payload } = to else {
    panic!("expected callable to lower to PtrKind::GcObject");
  };

  let Layout::Struct { fields, size, align } = store.layout(payload) else {
    panic!("expected closure payload layout to be Struct");
  };

  assert_eq!(size, 16);
  assert_eq!(align, 8);
  assert_eq!(fields.len(), 2);
  assert_eq!(fields[0].key, FieldKey::Internal("fn_ptr".to_string()));
  assert_eq!(fields[0].offset, 0);
  assert_eq!(fields[1].key, FieldKey::Internal("env".to_string()));
  assert_eq!(fields[1].offset, 8);

  assert!(
    matches!(store.layout(fields[0].layout), Layout::Ptr { to: types_ts_interned::PtrKind::Opaque }),
    "expected fn_ptr field to be an opaque pointer"
  );
  assert!(
    matches!(store.layout(fields[1].layout), Layout::Ptr { to: types_ts_interned::PtrKind::GcAny }),
    "expected env field to be a GC-managed pointer with unknown pointee layout"
  );

  // GcAny must be considered a GC pointer for trace planning.
  assert_eq!(store.gc_ptr_offsets(payload), vec![8]);
}

#[test]
fn closure_layout_ids_are_deterministic_across_stores() {
  use types_ts_interned::Signature;

  fn build() -> (std::sync::Arc<TypeStore>, types_ts_interned::TypeId) {
    let store = TypeStore::new();
    let primitives = store.primitive_ids();
    let sig = store.intern_signature(Signature::new(Vec::new(), primitives.number));
    let callable = store.intern_type(TypeKind::Callable {
      overloads: vec![sig],
    });
    (store, callable)
  }

  let (store_a, ty_a) = build();
  let (store_b, ty_b) = build();
  assert_eq!(ty_a, ty_b);

  let root_a = store_a.layout_of(ty_a);
  let root_b = store_b.layout_of(ty_b);
  assert_eq!(root_a, root_b);

  let graph_a = collect_layout_graph(store_a.as_ref(), root_a);
  let graph_b = collect_layout_graph(store_b.as_ref(), root_b);
  assert_eq!(graph_a, graph_b);
}
