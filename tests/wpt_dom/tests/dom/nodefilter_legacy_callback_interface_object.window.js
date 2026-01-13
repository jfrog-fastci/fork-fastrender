// META: script=/resources/testharness.js
//
// NodeFilter is a WebIDL "legacy callback interface object" and should be exposed as a built-in
// function object whose call steps always throw a TypeError (and which is not constructible).
//
// Spec: https://webidl.spec.whatwg.org/#legacy-callback-interface-object

test(() => {
  assert_equals(typeof NodeFilter, "function");
}, "NodeFilter is exposed as a function object");

test(() => {
  assert_equals(NodeFilter.FILTER_ACCEPT, 1);
  assert_equals(NodeFilter.FILTER_REJECT, 2);
  assert_equals(NodeFilter.FILTER_SKIP, 3);

  assert_equals(NodeFilter.SHOW_ALL, 0xFFFFFFFF);
  assert_equals(NodeFilter.SHOW_ELEMENT, 0x1);
  assert_equals(NodeFilter.SHOW_ATTRIBUTE, 0x2);
  assert_equals(NodeFilter.SHOW_TEXT, 0x4);
  assert_equals(NodeFilter.SHOW_CDATA_SECTION, 0x8);
  assert_equals(NodeFilter.SHOW_ENTITY_REFERENCE, 0x10);
  assert_equals(NodeFilter.SHOW_ENTITY, 0x20);
  assert_equals(NodeFilter.SHOW_PROCESSING_INSTRUCTION, 0x40);
  assert_equals(NodeFilter.SHOW_COMMENT, 0x80);
  assert_equals(NodeFilter.SHOW_DOCUMENT, 0x100);
  assert_equals(NodeFilter.SHOW_DOCUMENT_TYPE, 0x200);
  assert_equals(NodeFilter.SHOW_DOCUMENT_FRAGMENT, 0x400);
  assert_equals(NodeFilter.SHOW_NOTATION, 0x800);
}, "NodeFilter exposes DOM traversal constants as static properties");

test(() => {
  assert_throws_js(TypeError, () => {
    NodeFilter();
  });
}, "NodeFilter call throws TypeError");

test(() => {
  assert_throws_js(TypeError, () => {
    new NodeFilter();
  });
}, "NodeFilter construct throws TypeError");

