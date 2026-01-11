// META: script=/resources/testharness.js

test(() => {
  assert_true(URL.canParse("https://example.com/"));
  assert_true(URL.canParse("a", "https://example.com/dir/"));
  assert_false(URL.canParse("not a url"));
}, "URL.canParse reports whether a URL can be parsed");

test(() => {
  const abs = URL.parse("https://example.com");
  assert_true(abs !== null);
  assert_equals(abs.href, "https://example.com/");

  const rel = URL.parse("a", "https://example.com/dir/");
  assert_true(rel !== null);
  assert_equals(rel.href, "https://example.com/dir/a");

  assert_equals(URL.parse("not a url"), null);
}, "URL.parse returns a URL object or null on failure");

