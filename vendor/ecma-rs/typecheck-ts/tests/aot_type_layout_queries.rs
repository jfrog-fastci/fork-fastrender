mod common;

use std::cmp::Ordering;

use typecheck_ts::lib_support::CompilerOptions;
use typecheck_ts::{DefKind, FileKey, MemoryHost, Program};
use types_ts_interned::{AbiScalar, FieldKey, Layout, PropKey, PtrKind, TypeKind};

fn aot_host() -> MemoryHost {
  let mut host = MemoryHost::with_options(CompilerOptions {
    no_default_lib: true,
    ..CompilerOptions::default()
  });
  host.add_lib(common::core_globals_lib());
  host
}

fn def_in_file(program: &Program, file_id: typecheck_ts::FileId, name: &str) -> typecheck_ts::DefId {
  program
    .definitions_in_file(file_id)
    .into_iter()
    .find(|def| program.def_name(*def).as_deref() == Some(name))
    .unwrap_or_else(|| panic!("missing def {name}"))
}

#[test]
fn evaluate_type_interned_expands_type_alias_refs() {
  let mut host = aot_host();
  let file = FileKey::new("main.ts");
  host.insert(
    file.clone(),
    r#"
type Foo = { a: number; b: boolean };
const x: Foo = { a: 1, b: true };
"#,
  );

  let program = Program::new(host, vec![file.clone()]);
  let file_id = program.file_id(&file).expect("file id");
  let foo_def = def_in_file(&program, file_id, "Foo");
  assert!(
    matches!(program.def_kind(foo_def), Some(DefKind::TypeAlias(_))),
    "Foo should be a type alias"
  );

  let store = program.interned_type_store();
  let foo_ref = program.type_of_def_interned(foo_def);
  assert!(
    matches!(store.type_kind(foo_ref), TypeKind::Ref { .. }),
    "expected a named ref for Foo"
  );

  let evaluated = program.evaluate_type_interned(foo_ref);
  assert!(
    matches!(store.type_kind(evaluated), TypeKind::Object(_)),
    "expected Foo to evaluate to an object type, got {:?}",
    store.type_kind(evaluated)
  );
}

#[test]
fn layout_of_interned_for_ref_is_gc_object_with_deterministic_fields() {
  let mut host = aot_host();
  let file = FileKey::new("main.ts");
  host.insert(file.clone(), "type Foo = { a: number; b: boolean };");

  let program = Program::new(host, vec![file.clone()]);
  let file_id = program.file_id(&file).expect("file id");
  let foo_def = def_in_file(&program, file_id, "Foo");

  let store = program.interned_type_store();
  let foo_ref = program.type_of_def_interned(foo_def);
  let layout_id = program.layout_of_interned(foo_ref);
  let layout = store.layout(layout_id);

  let payload_layout_id = match layout {
    Layout::Ptr {
      to: PtrKind::GcObject { layout },
    } => layout,
    other => panic!("expected pointer-to-gc-object layout, got {other:?}"),
  };

  let payload_layout = store.layout(payload_layout_id);
  let Layout::Struct { fields, size, align } = payload_layout else {
    panic!("expected struct payload layout, got {payload_layout:?}");
  };

  assert_eq!(size, 16);
  assert_eq!(align, 8);
  assert_eq!(fields.len(), 2);

  let field_name = |key: &FieldKey| match key {
    FieldKey::Prop(PropKey::String(id)) => store.name(*id),
    other => panic!("expected string prop key, got {other:?}"),
  };

  assert_eq!(field_name(&fields[0].key), "a");
  assert_eq!(fields[0].offset, 0);
  assert_eq!(fields[0].size, 8);
  assert_eq!(fields[0].align, 8);
  assert_eq!(store.layout(fields[0].layout), Layout::Scalar { abi: AbiScalar::F64 });

  assert_eq!(field_name(&fields[1].key), "b");
  assert_eq!(fields[1].offset, 8);
  assert_eq!(fields[1].size, 1);
  assert_eq!(fields[1].align, 1);
  assert_eq!(store.layout(fields[1].layout), Layout::Scalar { abi: AbiScalar::Bool });
}

#[test]
fn union_members_and_layout_of_interned_match_tagged_union_ordering() {
  let mut host = aot_host();
  let file = FileKey::new("main.ts");
  host.insert(
    file.clone(),
    r#"
type U = { a: number } | { b: boolean };
"#,
  );

  let program = Program::new(host, vec![file.clone()]);
  let file_id = program.file_id(&file).expect("file id");
  let union_def = def_in_file(&program, file_id, "U");

  let store = program.interned_type_store();
  let union_ref = program.type_of_def_interned(union_def);
  let members = program.union_members_interned(union_ref);
  assert_eq!(members.len(), 2, "expected two union members");
  assert!(
    matches!(store.type_kind(members[0]), TypeKind::Object(_)),
    "expected object union member"
  );
  assert!(
    matches!(store.type_kind(members[1]), TypeKind::Object(_)),
    "expected object union member"
  );
  assert_eq!(
    store.type_cmp(members[0], members[1]),
    Ordering::Less,
    "union_members_interned should return members in TypeStore canonical order"
  );

  let layout_id = program.layout_of_interned(union_ref);
  let layout = store.layout(layout_id);
  let Layout::TaggedUnion {
    tag,
    payload_offset,
    variants,
    size,
    align,
  } = layout
  else {
    panic!("expected tagged union layout, got {layout:?}");
  };

  assert_eq!(tag.abi, AbiScalar::U8);
  assert_eq!(tag.offset, 0);
  assert_eq!(payload_offset, 8);
  assert_eq!(size, 16);
  assert_eq!(align, 8);
  assert_eq!(variants.len(), 2);

  for (idx, variant) in variants.iter().enumerate() {
    assert_eq!(variant.ty, members[idx]);
    assert_eq!(variant.discriminant, idx as u32);
    // `types-ts-interned` stores per-variant payload offsets *relative* to the
    // union's `payload_offset`, which is shared by all variants in the current
    // layout model.
    assert_eq!(variant.payload_offset, 0);
    assert!(matches!(
      store.layout(variant.layout),
      Layout::Ptr {
        to: PtrKind::GcObject { .. }
      }
    ));
  }
}
