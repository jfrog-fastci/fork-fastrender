// META: script=/resources/testharness.js
//
// Smoke tests for supporting legacy DOMException code strings with `assert_throws_dom`.

test(() => {
  const err = assert_throws_dom("INVALID_NODE_TYPE_ERR", () => {
    // Avoid depending on a full DOMException implementation so the test runs on
    // the lightweight QuickJS backend too.
    throw { name: "InvalidNodeTypeError" };
  });
  assert_equals(err.name, "InvalidNodeTypeError", "returns the thrown exception");
}, "assert_throws_dom(INVALID_NODE_TYPE_ERR) maps to InvalidNodeTypeError");

test(() => {
  const err = assert_throws_dom("INDEX_SIZE_ERR", () => {
    throw { name: "IndexSizeError" };
  });
  assert_equals(err.name, "IndexSizeError", "returns the thrown exception");
}, "assert_throws_dom(INDEX_SIZE_ERR) maps to IndexSizeError");
