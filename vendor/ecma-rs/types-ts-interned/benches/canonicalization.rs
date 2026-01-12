use criterion::{criterion_group, criterion_main, BatchSize, Criterion};
use ordered_float::OrderedFloat;
use std::hint::black_box;
use types_ts_interned::{
  Indexer, ObjectType, Param, PropData, PropKey, Property, Shape, Signature, SignatureId, TypeId,
  TypeKind, TypeStore,
};

fn intern_string_literals(store: &TypeStore, prefix: &str, count: usize) -> Vec<TypeId> {
  (0..count)
    .map(|idx| {
      let name = store.intern_name(format!("{prefix}{idx:04}"));
      store.intern_type(TypeKind::StringLiteral(name))
    })
    .collect()
}

fn intern_number_literals(store: &TypeStore, count: usize) -> Vec<TypeId> {
  (0..count)
    .map(|idx| store.intern_type(TypeKind::NumberLiteral(OrderedFloat::from(idx as f64))))
    .collect()
}

fn intern_unary_signatures(store: &TypeStore, param_types: &[TypeId], ret: TypeId) -> Vec<SignatureId> {
  param_types
    .iter()
    .copied()
    .map(|ty| {
      store.intern_signature(Signature::new(
        vec![Param {
          name: None,
          ty,
          optional: false,
          rest: false,
        }],
        ret,
      ))
    })
    .collect()
}

fn bench_canonicalization(c: &mut Criterion) {
  let store = TypeStore::new();
  let mut group = c.benchmark_group("types-ts-interned/canonicalization");

  let flat = intern_string_literals(&store, "u", 1024);
  let flat_dupes: Vec<_> = flat.iter().copied().chain(flat.iter().copied()).collect();
  group.bench_function("union/flat_2048_dupes", |b| {
    b.iter_batched(
      || flat_dupes.clone(),
      |members| black_box(store.union(members)),
      BatchSize::LargeInput,
    );
  });

  let nested_literals = intern_string_literals(&store, "n", 2048);
  let nested_unions: Vec<_> = nested_literals
    .chunks(8)
    .map(|chunk| store.intern_type(TypeKind::Union(chunk.to_vec())))
    .collect();
  group.bench_function("union/nested_256x8", |b| {
    b.iter_batched(
      || nested_unions.clone(),
      |members| black_box(store.union(members)),
      BatchSize::LargeInput,
    );
  });

  let nums = intern_number_literals(&store, 1024);
  let nums_dupes: Vec<_> = nums.iter().copied().chain(nums.iter().copied()).collect();
  group.bench_function("intersection/flat_2048_dupes", |b| {
    b.iter_batched(
      || nums_dupes.clone(),
      |members| black_box(store.intersection(members)),
      BatchSize::LargeInput,
    );
  });

  let nested_nums = intern_number_literals(&store, 2048);
  let nested_intersections: Vec<_> = nested_nums
    .chunks(8)
    .map(|chunk| store.intern_type(TypeKind::Intersection(chunk.to_vec())))
    .collect();
  group.bench_function("intersection/nested_256x8", |b| {
    b.iter_batched(
      || nested_intersections.clone(),
      |members| black_box(store.intersection(members)),
      BatchSize::LargeInput,
    );
  });

  // Structural types: object shapes with many properties + signatures.
  let primitives = store.primitive_ids();

  // Build a pool of signatures based on unique string literal parameter types.
  let sig_param_types = intern_string_literals(&store, "sig", 1024);
  let sig_pool = intern_unary_signatures(&store, &sig_param_types, primitives.number);

  // Create a shared property list (identical across shapes) so object comparisons
  // have to walk properties and then compare signatures.
  let prop_keys: Vec<_> = (0..64)
    .map(|idx| store.intern_name(format!("prop{idx:04}")))
    .collect();
  let base_properties: Vec<Property> = prop_keys
    .iter()
    .enumerate()
    .map(|(idx, key)| Property {
      key: PropKey::String(*key),
      data: PropData {
        ty: primitives.string,
        optional: idx % 3 != 0,
        readonly: idx % 7 == 0,
        accessibility: None,
        is_method: false,
        origin: None,
        declared_on: None,
      },
    })
    .collect();

  let object_types: Vec<TypeId> = (0..256)
    .map(|idx| {
      let mut shape = Shape::new();
      shape.properties = base_properties.clone();
      shape.call_signatures = (0..8).map(|o| sig_pool[idx + o]).collect();
      shape.construct_signatures = (0..4).map(|o| sig_pool[512 + idx + o]).collect();
      shape.indexers.push(Indexer {
        key_type: primitives.string,
        value_type: primitives.number,
        readonly: idx % 2 == 0,
      });
      let shape = store.intern_shape(shape);
      let object = store.intern_object(ObjectType { shape });
      store.intern_type(TypeKind::Object(object))
    })
    .collect();

  let object_union_members: Vec<_> = object_types
    .iter()
    .copied()
    .chain(object_types.iter().copied())
    .collect();
  // Warm-up so the resulting union is already interned during benchmarking.
  let _ = store.union(object_union_members.clone());
  group.bench_function("union/structural_objects_512_dupes", |b| {
    b.iter_batched(
      || object_union_members.clone(),
      |members| black_box(store.union(members)),
      BatchSize::LargeInput,
    );
  });

  let object_intersection_members: Vec<_> = object_types
    .iter()
    .copied()
    .chain(object_types.iter().copied())
    .collect();
  let _ = store.intersection(object_intersection_members.clone());
  group.bench_function("intersection/structural_objects_512_dupes", |b| {
    b.iter_batched(
      || object_intersection_members.clone(),
      |members| black_box(store.intersection(members)),
      BatchSize::LargeInput,
    );
  });

  // Callable members with many overloads.
  let callable_types: Vec<TypeId> = (0..256)
    .map(|idx| {
      let overloads: Vec<_> = (0..16).map(|o| sig_pool[idx + o]).collect();
      store.intern_type(TypeKind::Callable { overloads })
    })
    .collect();

  let callable_union_members: Vec<_> = callable_types
    .iter()
    .copied()
    .chain(callable_types.iter().copied())
    .collect();
  let _ = store.union(callable_union_members.clone());
  group.bench_function("union/structural_callables_512_dupes", |b| {
    b.iter_batched(
      || callable_union_members.clone(),
      |members| black_box(store.union(members)),
      BatchSize::LargeInput,
    );
  });

  // Shape canonicalization: heavy property duplicates + many signatures/indexers.
  let heavy_prop_keys: Vec<_> = (0..256)
    .map(|idx| store.intern_name(format!("heavy_prop{idx:04}")))
    .collect();
  let mut heavy_shape = Shape::new();
  for (idx, key) in heavy_prop_keys.iter().enumerate() {
    for dupe in 0..4 {
      heavy_shape.properties.push(Property {
        key: PropKey::String(*key),
        data: PropData {
          ty: primitives.number,
          optional: (idx + dupe) % 2 == 0,
          readonly: (idx + dupe) % 3 == 0,
          accessibility: None,
          is_method: false,
          origin: None,
          declared_on: None,
        },
      });
    }
  }
  // Many duplicate signatures to force dedup + expensive sort ordering.
  heavy_shape.call_signatures = sig_pool[..256]
    .iter()
    .copied()
    .chain(sig_pool[..256].iter().copied())
    .collect();
  heavy_shape.construct_signatures = sig_pool[256..512]
    .iter()
    .copied()
    .chain(sig_pool[256..512].iter().copied())
    .collect();
  // Many duplicate indexers keyed on `string`, but with distinct (structural)
  // value types to exercise `type_cmp` during ordering.
  for (idx, ty) in object_types.iter().take(64).enumerate() {
    heavy_shape.indexers.push(Indexer {
      key_type: primitives.string,
      value_type: *ty,
      readonly: idx % 2 == 0,
    });
  }
  // Duplicate the indexers to force merge.
  heavy_shape.indexers.extend(heavy_shape.indexers.clone());

  // Warm-up so all derived intersection/union results are already interned.
  let _ = store.intern_shape(heavy_shape.clone());
  group.bench_function("shape/intern_heavy_dupes", |b| {
    b.iter_batched(
      || heavy_shape.clone(),
      |shape| black_box(store.intern_shape(shape)),
      BatchSize::LargeInput,
    );
  });

  group.finish();
}

criterion_group!(benches, bench_canonicalization);
criterion_main!(benches);
