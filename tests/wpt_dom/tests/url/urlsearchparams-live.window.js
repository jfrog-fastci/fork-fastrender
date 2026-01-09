// META: script=/resources/testharness.js

test(() => {
  const url = new URL("https://example.com/?a=b%20~");
  assert_equals(url.search, "?a=b%20~", "URL.search preserves raw query on construction");

  const params = url.searchParams;
  assert_equals(params.get("a"), "b ~");
  assert_equals(params.toString(), "a=b+%7E", "URLSearchParams.toString normalizes encoding");

  // Reading searchParams does not rewrite URL.search.
  assert_equals(url.search, "?a=b%20~");

  // Mutating searchParams rewrites URL.search using application/x-www-form-urlencoded serialization.
  params.append("c", "d");
  assert_equals(url.search, "?a=b+%7E&c=d");
  assert_equals(url.href, "https://example.com/?a=b+%7E&c=d");
}, "URL.searchParams is live and mutating it updates URL.search + URL.href");

test(() => {
  const url = new URL("https://example.com/");
  const params = url.searchParams;

  url.search = "?q=a+b";
  assert_equals(url.search, "?q=a+b");
  assert_equals(params.get("q"), "a b");
  assert_equals(params.toString(), "q=a+b");
}, "Setting URL.search updates the associated URLSearchParams view");

