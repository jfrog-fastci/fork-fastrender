use effect_js::{ApiDatabase, EffectSet, Purity};

#[test]
fn array_map_purity_depends_on_callback() {
  let db = ApiDatabase::from_embedded().expect("embedded knowledge base loads");
  let map = db
    .get("Array.prototype.map")
    .expect("Array.prototype.map entry present");

  let effects = map.effects_for_call(&[EffectSet::empty()]);
  assert!(
    effects.contains(EffectSet::ALLOCATES),
    "expected Array.prototype.map to allocate, got {effects:?}"
  );
  assert!(
    effects.contains(EffectSet::MAY_THROW),
    "expected Array.prototype.map to include MAY_THROW, got {effects:?}"
  );

  assert_eq!(
    map.purity_for_call(&[Purity::Pure]),
    Purity::Allocating,
    "map should be allocating even with a pure callback",
  );
  assert_eq!(
    map.purity_for_call(&[Purity::Impure]),
    Purity::Impure,
    "map should be impure if the callback is impure",
  );
}

#[test]
fn promise_all_has_throw_effect_and_non_pure_purity() {
  let db = ApiDatabase::from_embedded().expect("embedded knowledge base loads");
  let api = db.get("Promise.all").expect("Promise.all entry present");

  let effects = api.effects_for_call(&[]);
  assert!(
    effects.contains(EffectSet::MAY_THROW),
    "expected Promise.all to include MAY_THROW, got {effects:?}",
  );

  let purity = api.purity_for_call(&[]);
  assert!(
    matches!(purity, Purity::ReadOnly | Purity::Allocating),
    "expected Promise.all to be at least ReadOnly/Allocating, got {purity:?}",
  );
}

#[test]
fn promise_then_depends_on_both_callbacks() {
  let db = ApiDatabase::from_embedded().expect("embedded knowledge base loads");
  let api = db
    .get("Promise.prototype.then")
    .expect("Promise.prototype.then entry present");

  let effects = api.effects_for_call(&[EffectSet::empty(), EffectSet::empty()]);
  assert!(
    effects.contains(EffectSet::ALLOCATES),
    "expected Promise.prototype.then to allocate, got {effects:?}",
  );
  assert!(
    effects.contains(EffectSet::NONDETERMINISTIC),
    "expected Promise.prototype.then to be nondeterministic, got {effects:?}",
  );
  assert!(
    effects.contains(EffectSet::MAY_THROW),
    "expected Promise.prototype.then to include MAY_THROW, got {effects:?}",
  );

  assert_eq!(
    api.purity_for_call(&[Purity::Pure, Purity::Pure]),
    Purity::Allocating,
    "then should be allocating with pure callbacks",
  );
  assert_eq!(
    api.purity_for_call(&[Purity::Impure, Purity::Pure]),
    Purity::Impure,
    "then should be impure if any callback is impure",
  );
}
