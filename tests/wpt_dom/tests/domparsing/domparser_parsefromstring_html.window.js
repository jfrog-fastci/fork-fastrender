// META: script=/resources/testharness.js

test(() => {
  assert_equals(typeof DOMParser, "function");
  assert_throws_js(TypeError, () => DOMParser());
  assert_true(new DOMParser() instanceof DOMParser);
}, "DOMParser constructor exists and requires 'new'");

test(() => {
  const doc = new DOMParser().parseFromString(
    "<!doctype html><html><body><div id=a>hi</div></body></html>",
    "text/html"
  );

  assert_true(doc instanceof Document);
  assert_equals(doc.URL, document.URL);
  assert_equals(doc.documentElement.tagName, "HTML");
  assert_equals(doc.body.tagName, "BODY");
  assert_equals(doc.getElementById("a").textContent, "hi");
}, "DOMParser.parseFromString(text/html) parses HTML into a Document");

test(() => {
  delete globalThis.__dp_ran;

  new DOMParser().parseFromString(
    "<!doctype html><html><body><script>globalThis.__dp_ran=true</script></body></html>",
    "text/html"
  );

  assert_equals(globalThis.__dp_ran, undefined);
}, "DOMParser.parseFromString(text/html) must not execute scripts");

