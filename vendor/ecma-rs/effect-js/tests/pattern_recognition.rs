use effect_js::{load_default_api_database, recognize_patterns_best_effort_untyped, GuardKind, RecognizedPattern};

fn recognize(source: &str) -> Vec<RecognizedPattern> {
  let lowered = hir_js::lower_from_source(source).expect("lower source");
  let kb = load_default_api_database();
  let mut out = Vec::new();
  for (body_id, _) in lowered.body_index.iter() {
    out.extend(recognize_patterns_best_effort_untyped(&kb, &lowered, *body_id));
  }
  out
}

#[test]
fn promise_all_fetch_urls_map_fetch_is_recognized() {
  let patterns = recognize("Promise.all(urls.map(url => fetch(url)));");

  assert!(patterns.iter().any(|pat| {
    matches!(
      pat,
      RecognizedPattern::PromiseAllFetch {
        map_call: Some(_),
        fetch_call_count: 1,
        ..
      }
    )
  }));
}

#[test]
fn promise_all_fetch_urls_map_fetch_ident_is_recognized() {
  let patterns = recognize("Promise.all(urls.map(fetch));");

  assert!(patterns.iter().any(|pat| {
    matches!(
      pat,
      RecognizedPattern::PromiseAllFetch {
        map_call: Some(_),
        fetch_call_count: 1,
        ..
      }
    )
  }));
}

#[test]
fn promise_all_fetch_urls_map_async_await_fetch_is_recognized() {
  let patterns = recognize("Promise.all(urls.map(async url => await fetch(url)));");

  assert!(patterns.iter().any(|pat| {
    matches!(
      pat,
      RecognizedPattern::PromiseAllFetch {
        map_call: Some(_),
        fetch_call_count: 1,
        ..
      }
    )
  }));
}

#[test]
fn promise_all_fetch_array_literal_is_recognized() {
  let patterns = recognize("Promise.all([fetch(a), fetch(b)]);");

  assert!(patterns.iter().any(|pat| {
    matches!(
      pat,
      RecognizedPattern::PromiseAllFetch {
        map_call: None,
        fetch_call_count: 2,
        ..
      }
    )
  }));
}

#[test]
fn async_iterator_for_await_is_recognized() {
  let patterns = recognize(
    r#"
async function run(asyncIterable: AsyncIterable<number>) {
  for await (const x of asyncIterable) {
    console.log(x);
  }
}
"#,
  );

  assert!(patterns
    .iter()
    .any(|pat| matches!(pat, RecognizedPattern::AsyncIterator { .. })));
}

#[test]
fn string_template_with_multiple_spans_is_recognized() {
  let patterns = recognize("const s = `${a} ${b}`;");

  assert!(patterns.iter().any(|pat| {
    matches!(
      pat,
      RecognizedPattern::StringTemplate {
        span_count: 2..,
        ..
      }
    )
  }));
}

#[test]
fn object_spread_is_recognized() {
  let patterns = recognize("const x = { ...a, x: 1 };");

  assert!(patterns.iter().any(|pat| {
    matches!(
      pat,
      RecognizedPattern::ObjectSpread {
        spread_count: 1..,
        ..
      }
    )
  }));
}

#[test]
fn array_destructure_is_recognized() {
  let patterns = recognize("const [a, b] = arr;");

  assert!(patterns.iter().any(|pat| {
    matches!(
      pat,
      RecognizedPattern::ArrayDestructure { arity: 2, .. }
    )
  }));
}

#[test]
fn guard_clause_return_is_recognized() {
  let patterns = recognize(
    r#"
function f(x?: number) {
  if (!x) return;
  return x;
}
"#,
  );

  assert!(patterns.iter().any(|pat| {
    matches!(
      pat,
      RecognizedPattern::GuardClause {
        kind: GuardKind::Return,
        ..
      }
    )
  }));
}

#[test]
fn guard_clause_throw_is_recognized() {
  let patterns = recognize(
    r#"
function f(x?: number) {
  if (!x) throw new Error();
  return x;
}
"#,
  );

  assert!(patterns.iter().any(|pat| {
    matches!(
      pat,
      RecognizedPattern::GuardClause {
        kind: GuardKind::Throw,
        ..
      }
    )
  }));
}
