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
  assert!(
    read_file.effect_summary.flags.contains(EffectSet::IO),
    "expected node:fs.readFile to have io effect summary",
  );

  let fetch = db.get("fetch").unwrap();
  assert!(
    fetch.effect_summary.flags.contains(EffectSet::IO),
    "expected fetch to have io effect summary",
  );
  assert!(
    fetch.effect_summary.flags.contains(EffectSet::NETWORK),
    "expected fetch to have network effect summary",
  );

  let join = db.get("node:path.join").unwrap();
  assert!(
    join.effect_summary.is_pure(),
    "expected node:path.join to be pure, got {join:?}",
  );
}
