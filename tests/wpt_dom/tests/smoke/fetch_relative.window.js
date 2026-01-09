// META: script=/resources/testharness.js
// META: script=/resources/meta_dep.js
//
// This is a tiny smoke test for FastRender's QuickJS `fetch` shims.

promise_test(async () => {
  const resp = await fetch("/x");
  assert_equals(resp.url, "https://web-platform.test/x");

  const rel = await fetch("foo");
  assert_equals(rel.url, "https://web-platform.test/smoke/foo");

  const req = new Request("/y");
  const resp2 = await fetch(req);
  assert_equals(resp2.url, "https://web-platform.test/y");
}, "fetch() resolves relative URLs against the document base URL");

test(() => {
  try {
    __fastrender_resolve_url("foo", null);
    assert_unreached("expected __fastrender_resolve_url to throw");
  } catch (e) {
    assert_equals(e && e.name, "TypeError");
  }
}, "relative URL without base throws TypeError");
