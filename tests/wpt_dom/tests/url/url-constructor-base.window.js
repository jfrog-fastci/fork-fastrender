// META: script=/resources/testharness.js

test(() => {
  const base = new URL("https://example.com/dir/");
  const url = new URL("a", base);
  assert_equals(url.href, "https://example.com/dir/a");
}, "URL constructor resolves a relative URL against a URL object base");

test(() => {
  const url = new URL("https://example.com");
  assert_equals(url.href, "https://example.com/");
  assert_equals(url.origin, "https://example.com");
  assert_equals(url.pathname, "/");
}, "URL parses basic components and normalizes the trailing slash");

test(() => {
  const url = new URL("https://example.com/");
  url.search = "q=1";
  assert_equals(url.search, "?q=1");
  assert_equals(url.href, "https://example.com/?q=1");
}, "Setting URL.search without a leading '?' normalizes to '?query'");

test(() => {
  let threw = false;
  try {
    new URL("not a url");
  } catch (_e) {
    threw = true;
  }
  assert_true(threw);
}, "URL constructor throws on invalid input without a base URL");

