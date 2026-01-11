use effect_js::{ApiDatabase, EffectSet, EffectTemplate, Purity, PurityTemplate};
use effect_model::ThrowBehavior;

#[test]
fn knowledge_base_loads_from_embedded() {
  let db = ApiDatabase::load_default().expect("load bundled knowledge base");

  let required = [
    "Array.prototype.map",
    "Array.prototype.filter",
    "Array.prototype.reduce",
    "Array.prototype.forEach",
    "Promise.all",
    "Promise.race",
    "JSON.parse",
    "JSON.stringify",
    "String.prototype.toLowerCase",
    "String.prototype.split",
    "Math.sqrt",
    "Math.floor",
    "fetch",
  ];

  for name in required {
    assert!(
      db.get(name).is_some(),
      "required API not found in bundled knowledge-base: {name}"
    );
  }

  let json_parse = db.get("JSON.parse").expect("JSON.parse present");
  assert_ne!(json_parse.effect_summary.throws, ThrowBehavior::Never);

  let array_map = db
    .get("Array.prototype.map")
    .expect("Array.prototype.map present");
  assert!(array_map.effect_summary.flags.contains(EffectSet::ALLOCATES));
  match &array_map.effects {
    EffectTemplate::DependsOnArgs { base, args } => {
      assert!(base.contains(EffectSet::ALLOCATES));
      assert_eq!(args.as_slice(), &[0]);
    }
    other => panic!("expected DependsOnArgs for Array.prototype.map, got {other:?}"),
  }
  assert_eq!(
    array_map.purity,
    PurityTemplate::DependsOnArgs {
      base: Purity::Allocating,
      args: vec![0],
    }
  );

  let fetch = db.get("fetch").expect("fetch present");
  assert!(fetch.effect_summary.flags.contains(EffectSet::IO));
}
