// META: script=/resources/testharness.js
// META: script=/resources/meta_dep.js
//
// This is a tiny smoke test for FastRender's `fetch`/`Request`/`Response` shims.

promise_test(() => {
  return fetch("/x")
    .then((resp) => {
      assert_equals(resp.url, "https://web-platform.test/x", "fetch('/x') response url");
      return fetch("foo");
    })
    .then((rel) => {
      assert_equals(rel.url, "https://web-platform.test/smoke/foo", "fetch('foo') response url");
      const req = new Request("/y");
      return fetch(req);
    })
    .then((resp2) => {
      assert_equals(resp2.url, "https://web-platform.test/y", "fetch(Request) response url");
    });
}, "fetch() resolves relative URLs against the document base URL");

test(() => {
  try {
    __fastrender_resolve_url("foo", null);
    assert_unreached("expected __fastrender_resolve_url to throw");
  } catch (e) {
    assert_true(e !== null, "expected thrown value to be non-null");
    assert_equals(e.name, "TypeError", "__fastrender_resolve_url throws TypeError");
  }
}, "relative URL without base throws TypeError");
