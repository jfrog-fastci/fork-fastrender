// META: script=/resources/testharness.js

test(() => {
  assert_equals(typeof document.body.baseURI, "string");
  assert_equals(document.body.baseURI, document.baseURI);

  // The accessor must enforce `Node` branding (i.e. throw on non-Node receivers).
  try {
    Node.prototype.baseURI;
    assert_unreached("expected Node.prototype.baseURI to throw");
  } catch (e) {
    assert_true(e instanceof TypeError);
  }
}, "Node.baseURI reflects document.baseURI and enforces receiver branding");
