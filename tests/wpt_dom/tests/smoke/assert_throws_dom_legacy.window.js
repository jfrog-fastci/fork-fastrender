// META: script=/resources/testharness.js
//
// Smoke tests for supporting legacy DOMException code strings with `assert_throws_dom`.

test(() => {
  const err = assert_throws_dom("INVALID_NODE_TYPE_ERR", () => {
    // Avoid depending on a full DOMException implementation; we only need a `.name` property to
    // exercise `assert_throws_dom`'s legacy name mapping.
    throw { name: "InvalidNodeTypeError" };
  });
  assert_equals(err.name, "InvalidNodeTypeError", "returns the thrown exception");
}, "assert_throws_dom(INVALID_NODE_TYPE_ERR) maps to InvalidNodeTypeError");

test(() => {
  const err = assert_throws_dom("WRONG_DOCUMENT_ERR", () => {
    throw { name: "WrongDocumentError" };
  });
  assert_equals(err.name, "WrongDocumentError", "returns the thrown exception");
}, "assert_throws_dom(WRONG_DOCUMENT_ERR) maps to WrongDocumentError");

test(() => {
  const err = assert_throws_dom("NOT_SUPPORTED_ERR", () => {
    throw { name: "NotSupportedError" };
  });
  assert_equals(err.name, "NotSupportedError", "returns the thrown exception");
}, "assert_throws_dom(NOT_SUPPORTED_ERR) maps to NotSupportedError");

test(() => {
  const err = assert_throws_dom("INDEX_SIZE_ERR", () => {
    throw { name: "IndexSizeError" };
  });
  assert_equals(err.name, "IndexSizeError", "returns the thrown exception");
}, "assert_throws_dom(INDEX_SIZE_ERR) maps to IndexSizeError");

test(() => {
  const err = assert_throws_dom("IndexSizeError", () => {
    throw { name: "INDEX_SIZE_ERR" };
  });
  assert_equals(err.name, "INDEX_SIZE_ERR", "returns the thrown exception");
}, "assert_throws_dom(IndexSizeError) matches INDEX_SIZE_ERR (legacy thrown name)");

test(() => {
  const err = assert_throws_dom("HIERARCHY_REQUEST_ERR", () => {
    throw { name: "HierarchyRequestError" };
  });
  assert_equals(err.name, "HierarchyRequestError", "returns the thrown exception");
}, "assert_throws_dom(HIERARCHY_REQUEST_ERR) maps to HierarchyRequestError");

test(() => {
  const err = assert_throws_dom("NO_MODIFICATION_ALLOWED_ERR", () => {
    throw { name: "NoModificationAllowedError" };
  });
  assert_equals(err.name, "NoModificationAllowedError", "returns the thrown exception");
}, "assert_throws_dom(NO_MODIFICATION_ALLOWED_ERR) maps to NoModificationAllowedError");

test(() => {
  const err = assert_throws_dom("NAMESPACE_ERR", () => {
    throw { name: "NamespaceError" };
  });
  assert_equals(err.name, "NamespaceError", "returns the thrown exception");
}, "assert_throws_dom(NAMESPACE_ERR) maps to NamespaceError");

test(() => {
  const err = assert_throws_dom("SYNTAX_ERR", () => {
    throw { name: "SyntaxError" };
  });
  assert_equals(err.name, "SyntaxError", "returns the thrown exception");
}, "assert_throws_dom(SYNTAX_ERR) maps to SyntaxError");

test(() => {
  const err = assert_throws_dom("INVALID_CHARACTER_ERR", () => {
    throw { name: "InvalidCharacterError" };
  });
  assert_equals(err.name, "InvalidCharacterError", "returns the thrown exception");
}, "assert_throws_dom(INVALID_CHARACTER_ERR) maps to InvalidCharacterError");

test(() => {
  const err = assert_throws_dom("NOT_FOUND_ERR", () => {
    throw { name: "NotFoundError" };
  });
  assert_equals(err.name, "NotFoundError", "returns the thrown exception");
}, "assert_throws_dom(NOT_FOUND_ERR) maps to NotFoundError");

test(() => {
  const err = assert_throws_dom("INVALID_STATE_ERR", () => {
    throw { name: "InvalidStateError" };
  });
  assert_equals(err.name, "InvalidStateError", "returns the thrown exception");
}, "assert_throws_dom(INVALID_STATE_ERR) maps to InvalidStateError");

test(() => {
  // Some JS DOM shims (or older DOMException implementations) can surface legacy names via
  // `.name`. Ensure we accept those when the expected value is the modern name.
  const err = assert_throws_dom("InvalidNodeTypeError", () => {
    throw { name: "INVALID_NODE_TYPE_ERR" };
  });
  assert_equals(err.name, "INVALID_NODE_TYPE_ERR", "returns the thrown exception unmodified");
}, "assert_throws_dom(InvalidNodeTypeError) accepts legacy thrown name INVALID_NODE_TYPE_ERR");

test(() => {
  // Ensure the legacy mapping doesn't accidentally accept the wrong error.
  assert_throws_js(Error, () => {
    assert_throws_dom("INVALID_NODE_TYPE_ERR", () => {
      throw { name: "IndexSizeError" };
    });
  });
}, "assert_throws_dom legacy mapping rejects mismatched errors");
