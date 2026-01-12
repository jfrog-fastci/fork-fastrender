use std::sync::Arc;

use hir_js::DefKind as HirDefKind;
use types_ts_interned::TypeKind;

use typecheck_ts::db::queries;
use typecheck_ts::db::{Db, TypecheckDb};
use typecheck_ts::lib_support::FileKind;
use typecheck_ts::{FileId, FileKey, FileOrigin};

fn def_in_file(db: &TypecheckDb, file: FileId, name: &str, kind: HirDefKind) -> hir_js::DefId {
  let lowered = db
    .lower_hir(file)
    .lowered
    .expect("expected file to lower without fatal errors");
  lowered
    .defs
    .iter()
    .find(|def| def.path.kind == kind && lowered.names.resolve(def.name) == Some(name))
    .map(|def| def.id)
    .unwrap_or_else(|| panic!("definition {name} ({kind:?}) not found in file {file:?}"))
}

#[test]
fn typeof_class_uses_salsa_value_defs_after_incremental_edit() {
  let mut db = TypecheckDb::default();
  let file = FileId(0);
  let key = FileKey::new("main.ts");
  db.set_file(
    file,
    key.clone(),
    FileKind::Ts,
    Arc::from("class C { static x: number = 1 }\ntype T = typeof C;\n"),
    FileOrigin::Source,
  );
  db.set_roots(Arc::from([key.clone()]));

  let class_def = def_in_file(&db, file, "C", HirDefKind::Class);
  let alias_def = def_in_file(&db, file, "T", HirDefKind::TypeAlias);

  let value_defs = queries::value_defs(&db);
  let value_def = *value_defs
    .get(&class_def)
    .expect("expected class to have synthesized value def");
  assert_ne!(
    value_def, class_def,
    "synthesized value def should differ from class type-side def"
  );

  let decls = db.decl_types(file);
  let alias_ty = decls
    .types
    .get(&alias_def)
    .copied()
    .expect("expected decl types to contain typeof alias");
  let store = db.type_store_input().store(&db).arc();
  let alias_target = match store.type_kind(alias_ty) {
    TypeKind::Ref { def, .. } => def,
    other => panic!("expected typeof alias to lower to a ref, got {other:?}"),
  };
  assert_eq!(alias_target.0, value_def.0);

  db.set_file_text(
    file,
    Arc::from("class C { static x: string = \"hi\" }\ntype T = typeof C;\n"),
  );

  let class_def = def_in_file(&db, file, "C", HirDefKind::Class);
  let alias_def = def_in_file(&db, file, "T", HirDefKind::TypeAlias);
  let value_defs = queries::value_defs(&db);
  let value_def = *value_defs
    .get(&class_def)
    .expect("expected class to have synthesized value def after edit");

  let decls = db.decl_types(file);
  let alias_ty = decls
    .types
    .get(&alias_def)
    .copied()
    .expect("expected decl types to contain typeof alias after edit");
  let store = db.type_store_input().store(&db).arc();
  let alias_target = match store.type_kind(alias_ty) {
    TypeKind::Ref { def, .. } => def,
    other => panic!("expected typeof alias to lower to a ref after edit, got {other:?}"),
  };
  assert_eq!(alias_target.0, value_def.0);
}

#[test]
fn typeof_import_module_namespace_def_is_stable_across_text_edits() {
  let mut db = TypecheckDb::default();
  let entry_file = FileId(0);
  let entry_key = FileKey::new("entry.ts");
  let dep_file = FileId(1);
  let dep_key = FileKey::new("dep.ts");

  db.set_file(
    entry_file,
    entry_key.clone(),
    FileKind::Ts,
    Arc::from("type M = typeof import(\"./dep\");\n"),
    FileOrigin::Source,
  );
  db.set_file(
    dep_file,
    dep_key.clone(),
    FileKind::Ts,
    Arc::from("export const value: number = 1;\n"),
    FileOrigin::Source,
  );
  db.set_roots(Arc::from([entry_key.clone()]));
  db.set_module_resolution_ref(entry_file, "./dep", Some(dep_file));

  let alias_def = def_in_file(&db, entry_file, "M", HirDefKind::TypeAlias);
  let namespace_defs = queries::module_namespace_defs(&db);
  let namespace_def = *namespace_defs
    .get(&dep_file)
    .expect("expected dep file to have module namespace def");

  let decls = db.decl_types(entry_file);
  let alias_ty = decls
    .types
    .get(&alias_def)
    .copied()
    .expect("expected decl types to contain typeof import alias");
  let store = db.type_store_input().store(&db).arc();
  let alias_target = match store.type_kind(alias_ty) {
    TypeKind::Ref { def, .. } => def,
    other => panic!("expected typeof import alias to lower to a ref, got {other:?}"),
  };
  assert_eq!(alias_target.0, namespace_def.0);

  // A text edit that does not affect the file graph should not change the
  // synthetic module namespace def id.
  db.set_file_text(dep_file, Arc::from("export const value: number = 2;\n"));
  let namespace_defs_after = queries::module_namespace_defs(&db);
  assert_eq!(namespace_defs_after.get(&dep_file), Some(&namespace_def));

  let decls = db.decl_types(entry_file);
  let alias_ty = decls
    .types
    .get(&alias_def)
    .copied()
    .expect("expected decl types to contain typeof import alias after edit");
  let alias_target = match store.type_kind(alias_ty) {
    TypeKind::Ref { def, .. } => def,
    other => panic!("expected typeof import alias to lower to a ref after edit, got {other:?}"),
  };
  assert_eq!(alias_target.0, namespace_def.0);
}

#[test]
fn synthetic_def_maps_are_deterministic_across_root_order() {
  let file_a = FileId(0);
  let key_a = FileKey::new("a.ts");
  let file_b = FileId(1);
  let key_b = FileKey::new("b.ts");

  let seed_db = |roots: Arc<[FileKey]>| {
    let mut db = TypecheckDb::default();
    db.set_file(
      file_a,
      key_a.clone(),
      FileKind::Ts,
      Arc::from("export class A {}\n"),
      FileOrigin::Source,
    );
    db.set_file(
      file_b,
      key_b.clone(),
      FileKind::Ts,
      Arc::from("export enum E { A = 1 }\n"),
      FileOrigin::Source,
    );
    db.set_roots(roots);
    db
  };

  let db_a = seed_db(Arc::from([key_a.clone(), key_b.clone()]));
  let db_b = seed_db(Arc::from([key_b.clone(), key_a.clone()]));

  assert_eq!(
    queries::value_defs(&db_a).as_ref(),
    queries::value_defs(&db_b).as_ref(),
    "value def mapping should not depend on root order"
  );
  assert_eq!(
    queries::module_namespace_defs(&db_a).as_ref(),
    queries::module_namespace_defs(&db_b).as_ref(),
    "module namespace def mapping should not depend on root order"
  );
}

fn seed_db() -> TypecheckDb {
  let mut db = TypecheckDb::default();
  let entry_file = FileId(0);
  let entry_key = FileKey::new("entry.ts");
  let dep_file = FileId(1);
  let dep_key = FileKey::new("dep.ts");

  db.set_file(
    entry_file,
    entry_key.clone(),
    FileKind::Ts,
    Arc::from("type M = typeof import(\"./dep\");\nclass C {}\n"),
    FileOrigin::Source,
  );
  db.set_file(
    dep_file,
    dep_key.clone(),
    FileKind::Ts,
    Arc::from("export const value: number = 1;\nenum E { A = 1 }\n"),
    FileOrigin::Source,
  );
  db.set_roots(Arc::from([entry_key]));
  db.set_module_resolution_ref(entry_file, "./dep", Some(dep_file));
  db
}

#[test]
fn synthetic_def_queries_are_deterministic_across_runs() {
  let db_a = seed_db();
  let db_b = seed_db();

  assert_eq!(
    queries::value_defs(&db_a).as_ref(),
    queries::value_defs(&db_b).as_ref()
  );
  assert_eq!(
    queries::module_namespace_defs(&db_a).as_ref(),
    queries::module_namespace_defs(&db_b).as_ref()
  );
}

#[test]
fn value_defs_update_when_class_or_enum_added_or_removed() {
  let mut db = TypecheckDb::default();
  let file = FileId(0);
  let key = FileKey::new("main.ts");
  db.set_file(
    file,
    key.clone(),
    FileKind::Ts,
    Arc::from("export const value = 1;"),
    FileOrigin::Source,
  );
  db.set_roots(Arc::from([key]));

  assert!(
    queries::value_defs(&db).is_empty(),
    "expected no class/enum value defs in initial program"
  );

  db.set_file_text(
    file,
    Arc::from("export const value = 1;\nclass C {}\nenum E { A = 1 }\n"),
  );

  let class_def = def_in_file(&db, file, "C", HirDefKind::Class);
  let enum_def = def_in_file(&db, file, "E", HirDefKind::Enum);
  let defs = queries::value_defs(&db);
  assert!(
    defs.contains_key(&class_def),
    "expected value def mapping for class C"
  );
  assert!(
    defs.contains_key(&enum_def),
    "expected value def mapping for enum E"
  );

  db.set_file_text(file, Arc::from("export const value = 1;"));
  let defs = queries::value_defs(&db);
  assert!(
    !defs.contains_key(&class_def) && !defs.contains_key(&enum_def),
    "expected class/enum value defs to be removed after deleting declarations"
  );
}
