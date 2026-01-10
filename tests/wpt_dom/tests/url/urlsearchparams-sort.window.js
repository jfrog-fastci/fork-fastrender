// META: script=/resources/testharness.js

test(() => {
  const params = new URLSearchParams("b=2&a=1&b=1");
  params.sort();
  assert_equals(params.toString(), "a=1&b=2&b=1");
}, "URLSearchParams.sort orders pairs by name (stable for duplicates)");

test(() => {
  const params = new URLSearchParams("a=1&a=2&b=3");
  assert_equals(params.getAll("a").join(","), "1,2");

  params.set("a", "x");
  assert_equals(params.toString(), "a=x&b=3");
}, "URLSearchParams.getAll returns duplicates and set replaces them");

