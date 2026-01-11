// META: script=/resources/testharness.js

test(() => {
  const params = new URLSearchParams("a=1&b=2&a=3");

  assert_equals(params.size, 3, "size reports number of pairs");
  assert_true(params.has("a"));
  assert_false(params.has("c"));

  assert_true(params.has("a", "1"));
  assert_false(params.has("a", "x"));

  // Value-aware delete removes only matching pairs.
  params.delete("a", "1");
  assert_equals(params.toString(), "b=2&a=3");
  assert_false(params.has("a", "1"));
  assert_true(params.has("a", "3"));

  // Name-only delete removes all pairs for that name.
  params.delete("b");
  assert_equals(params.toString(), "a=3");
  assert_equals(params.size, 1);
}, "URLSearchParams.has/delete/size behave per WHATWG URL");

test(() => {
  const params = new URLSearchParams("a=1&b=2&a=3");

  const keys_iter = params.keys();
  const keys = [];
  let step;
  for (;;) {
    step = keys_iter.next();
    if (step.done) break;
    keys.push(step.value);
  }
  assert_equals(keys.length, 3);
  assert_equals(keys[0], "a");
  assert_equals(keys[1], "b");
  assert_equals(keys[2], "a");

  const values_iter = params.values();
  const values = [];
  for (;;) {
    step = values_iter.next();
    if (step.done) break;
    values.push(step.value);
  }
  assert_equals(values.length, 3);
  assert_equals(values[0], "1");
  assert_equals(values[1], "2");
  assert_equals(values[2], "3");

  const entries_iter = params.entries();
  const entries = [];
  for (;;) {
    step = entries_iter.next();
    if (step.done) break;
    entries.push(step.value);
  }

  assert_equals(entries.length, 3);
  assert_equals(entries[0][0], "a");
  assert_equals(entries[0][1], "1");
  assert_equals(entries[1][0], "b");
  assert_equals(entries[1][1], "2");
  assert_equals(entries[2][0], "a");
  assert_equals(entries[2][1], "3");
}, "URLSearchParams iterators (keys/values/entries) yield pairs in order");

test(() => {
  const params = new URLSearchParams("a=1&b=2&a=3");

  const receiver = {};
  const calls = [];
  params.forEach(function (value, name, sp) {
    assert_equals(this, receiver);
    assert_equals(sp, params);
    calls.push([name, value]);
  }, receiver);

  assert_equals(calls.length, 3);
  assert_equals(calls[0][0], "a");
  assert_equals(calls[0][1], "1");
  assert_equals(calls[1][0], "b");
  assert_equals(calls[1][1], "2");
  assert_equals(calls[2][0], "a");
  assert_equals(calls[2][1], "3");
}, "URLSearchParams.forEach invokes callback(value, name, this)");

test(() => {
  const url = new URL("https://example.com/?a=1&b=2");
  url.searchParams.delete("a");
  assert_equals(url.search, "?b=2");
  assert_equals(url.href, "https://example.com/?b=2");
}, "Mutating URL.searchParams via delete updates URL.search + URL.href");
