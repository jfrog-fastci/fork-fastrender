#![cfg(feature = "serde")]

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use typecheck_ts::db::queries::{type_of_def, type_store};
use typecheck_ts::db::{DeclInfo, DeclKind, SharedTypeStore, TypesDatabase};
use typecheck_ts::{FileId, FileKey, MemoryHost, Program};
use types_ts_interned::{DefId, TypeDisplay, TypeStore};

fn seed_host() -> (MemoryHost, FileKey, FileKey) {
  let mut host = MemoryHost::new();
  let file_a = FileKey::new("a.ts");
  let file_b = FileKey::new("b.ts");
  host.insert(
    file_a.clone(),
    "export interface Box<T> { value: T; }\nexport type Alias = Box<number>;",
  );
  host.insert(
    file_b.clone(),
    "import { Box } from \"./a\";\n\
     export type MapBox<T> = (box: Box<T>) => T;\n\
     export interface Wrapper<U> extends Box<U> { wrapped: U; }",
  );
  host.link(file_b.clone(), "./a", file_a.clone());
  (host, file_a, file_b)
}

#[test]
fn decl_queries_match_program_types() {
  let (host, file_a, file_b) = seed_host();
  let program_host = host.clone();
  let program = Program::new(program_host, vec![file_b.clone()]);
  let diagnostics = program.check();
  assert!(
    diagnostics.is_empty(),
    "program diagnostics should be empty: {diagnostics:?}"
  );

  let file_a_id = program.file_id(&file_a).expect("file a id");
  let file_b_id = program.file_id(&file_b).expect("file b id");

  let exports_a = program.exports_of(file_a_id);
  let exports_b = program.exports_of(file_b_id);
  let box_def = exports_a
    .get("Box")
    .and_then(|entry| entry.def)
    .expect("box def");
  let map_def = exports_b
    .get("MapBox")
    .and_then(|entry| entry.def)
    .expect("MapBox def");
  let wrapper_def = exports_b
    .get("Wrapper")
    .and_then(|entry| entry.def)
    .expect("wrapper def");

  let program_box_ty = program.type_of_def_interned(box_def);
  let program_map_ty = program.type_of_def_interned(map_def);
  let program_wrapper_ty = program.type_of_def_interned(wrapper_def);

  // This test exercises the standalone `TypesDatabase` query engine by seeding
  // it with the declared types for a small program and asserting that
  // `type_of_def` matches the main `Program` API.
  //
  // Do not iterate over every definition from the default lib set: TypeScript's
  // bundled `.d.ts` files contain thousands of declarations and many deeply
  // recursive types. Walking all of them (and forcing every type to intern) is
  // both unnecessary for this test and can lead to pathologically long runtimes
  // in debug builds.
  let compiler_options = program.compiler_options();
  let interned_store_snapshot = program.interned_type_store().snapshot();

  let mut decls_by_file: BTreeMap<FileId, BTreeMap<DefId, DeclInfo>> = BTreeMap::new();
  decls_by_file
    .entry(file_a_id)
    .or_default()
    .insert(
      box_def,
      DeclInfo {
        file: file_a_id,
        name: "Box".to_string(),
        kind: DeclKind::Interface,
        declared_type: Some(program_box_ty),
        initializer: None,
      },
    );
  decls_by_file
    .entry(file_b_id)
    .or_default()
    .insert(
      map_def,
      DeclInfo {
        file: file_b_id,
        name: "MapBox".to_string(),
        kind: DeclKind::TypeAlias,
        declared_type: Some(program_map_ty),
        initializer: None,
      },
    );
  decls_by_file
    .entry(file_b_id)
    .or_default()
    .insert(
      wrapper_def,
      DeclInfo {
        file: file_b_id,
        name: "Wrapper".to_string(),
        kind: DeclKind::Interface,
        declared_type: Some(program_wrapper_ty),
        initializer: None,
      },
    );

  let mut db = TypesDatabase::new();
  db.set_compiler_options(compiler_options);
  db.set_type_store(SharedTypeStore(TypeStore::from_snapshot(
    interned_store_snapshot,
  )));
  db.set_files(Arc::new(vec![file_a_id, file_b_id]));
  for (file, decls) in decls_by_file {
    db.set_decl_types_in_file(file, Arc::new(decls));
  }

  let store = type_store(&db).arc();
  let resolver_names: Arc<HashMap<DefId, String>> = Arc::new(
    [
      (box_def, "Box".to_string()),
      (map_def, "MapBox".to_string()),
      (wrapper_def, "Wrapper".to_string()),
    ]
    .into_iter()
    .collect(),
  );
  let resolver: Arc<dyn Fn(DefId) -> Option<String> + Send + Sync> = {
    let names = Arc::clone(&resolver_names);
    Arc::new(move |def: DefId| names.get(&def).cloned())
      as Arc<dyn Fn(DefId) -> Option<String> + Send + Sync>
  };

  let decl_box = type_of_def(&db, box_def, ());
  let decl_box_str = TypeDisplay::new(store.as_ref(), decl_box)
    .with_ref_resolver(Arc::clone(&resolver))
    .to_string();
  let program_box_str = program.display_type(program_box_ty).to_string();
  assert_eq!(decl_box_str, program_box_str, "box type mismatch");

  let decl_map = type_of_def(&db, map_def, ());
  let decl_map_str = TypeDisplay::new(store.as_ref(), decl_map)
    .with_ref_resolver(Arc::clone(&resolver))
    .to_string();
  let program_map_str = program.display_type(program_map_ty).to_string();
  assert_eq!(decl_map_str, program_map_str, "MapBox type mismatch");

  let decl_wrapper = type_of_def(&db, wrapper_def, ());
  let decl_wrapper_str = TypeDisplay::new(store.as_ref(), decl_wrapper)
    .with_ref_resolver(resolver)
    .to_string();
  let program_wrapper_str = program.display_type(program_wrapper_ty).to_string();
  assert_eq!(
    decl_wrapper_str, program_wrapper_str,
    "wrapper interface type mismatch"
  );
}
