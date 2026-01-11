use effect_js::{ApiDatabase, EffectSet};

#[test]
fn embedded_knowledge_base_contains_node_and_web_entries() {
  let db = ApiDatabase::from_embedded().expect("knowledge base loads");

  // Node.js builtins.
  for name in [
    "node:fs.readFile",
    "node:fs.readFileSync",
    "node:fs.writeFile",
    "node:fs.writeFileSync",
    "node:fs.existsSync",
    "node:fs.stat",
    "node:fs.statSync",
    "node:path.join",
    "node:path.resolve",
    "node:path.basename",
    "node:path.dirname",
    "node:path.extname",
    "node:crypto.randomBytes",
    "node:buffer.Buffer.from",
  ] {
    assert!(db.get(name).is_some(), "missing KB entry: {name}");
  }

  // Web platform globals.
  for name in ["fetch", "URL", "URL.prototype.pathname", "URLSearchParams"] {
    assert!(db.get(name).is_some(), "missing KB entry: {name}");
  }

  // Spot-check a few conservative effect tags.
  let read_file = db.get("node:fs.readFile").unwrap();
  let read_file_effects = read_file.effects_for_call(&[]);
  assert!(
    read_file_effects.contains(EffectSet::IO),
    "expected node:fs.readFile to have io effect, got {read_file_effects:?}",
  );

  let fetch = db.get("fetch").unwrap();
  let fetch_effects = fetch.effects_for_call(&[]);
  assert!(
    fetch_effects.contains(EffectSet::IO),
    "expected fetch to have io effect, got {fetch_effects:?}",
  );
  assert!(
    fetch_effects.contains(EffectSet::NETWORK),
    "expected fetch to have network effect, got {fetch_effects:?}",
  );

  let join = db.get("node:path.join").unwrap();
  let join_effects = join.effects_for_call(&[]);
  assert!(
    join_effects.is_empty(),
    "expected node:path.join to be pure, got {join_effects:?}",
  );
}
