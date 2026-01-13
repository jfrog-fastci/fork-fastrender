// META: script=/resources/testharness.js

test(() => {
  assert_throws_dom("HIERARCHY_REQUEST_ERR", () => {
    throw { name: "HierarchyRequestError", code: 3 };
  });
}, "assert_throws_dom accepts legacy HIERARCHY_REQUEST_ERR expected name");

test(() => {
  assert_throws_dom("INDEX_SIZE_ERR", () => {
    throw { name: "IndexSizeError", code: 1 };
  });
}, "assert_throws_dom accepts legacy INDEX_SIZE_ERR expected name");

test(() => {
  assert_throws_dom("IndexSizeError", () => {
    throw { name: "IndexSizeError", code: 1 };
  });
}, "assert_throws_dom continues to accept modern DOMException names");

test(() => {
  // Some engines/tests may throw the legacy name even when the test expects the modern one.
  assert_throws_dom("IndexSizeError", () => {
    throw { name: "INDEX_SIZE_ERR", code: 1 };
  });
}, "assert_throws_dom accepts legacy thrown names when expecting modern names");

test(() => {
  // Legacy WPT tests sometimes assert the historical constant name while engines surface only
  // the numeric `.code`. Accept the match via `.code` even if `.name` is unexpected.
  assert_throws_dom("INDEX_SIZE_ERR", () => {
    throw { name: "UnexpectedErrorName", code: 1 };
  });
}, "assert_throws_dom matches legacy expected names against DOMException.code");

