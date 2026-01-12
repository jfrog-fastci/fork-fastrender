// META: script=/resources/testharness.js
//
// Live Range updates for CharacterData mutations.
//
// These tests exercise the DOM "replace data" algorithm via `text.data = ...` and verify that
// live Range offsets are updated accordingly.
//
// https://dom.spec.whatwg.org/#concept-cd-replace

function clear_children(node) {
  while (node.childNodes.length !== 0) {
    node.removeChild(node.childNodes[0]);
  }
}

test(() => {
  clear_children(document.body);

  const text = document.createTextNode("hello");
  document.body.appendChild(text);

  const range = document.createRange();
  range.setStart(text, 1);
  range.setEnd(text, 5);
  assert_equals(range.startContainer, text);
  assert_equals(range.startOffset, 1);
  assert_equals(range.endContainer, text);
  assert_equals(range.endOffset, 5);

  // CharacterData.data setter runs "replace data" with offset=0 and count=length.
  // Per the spec, any boundary point inside the replaced range (0 < offset <= length) is moved to
  // offset 0.
  text.data = "x";
  assert_equals(text.data, "x", "sanity: text data updated");

  assert_equals(range.startContainer, text, "startContainer should remain the same Text node");
  assert_equals(range.startOffset, 0, "startOffset should be updated by replace data");
  assert_equals(range.endContainer, text, "endContainer should remain the same Text node");
  assert_equals(range.endOffset, 0, "endOffset should be updated by replace data");
  assert_true(range.collapsed, "range should become collapsed after data replacement");
}, "Live Range offsets update when CharacterData.data is replaced (offsets inside replaced range move to 0)");

test(() => {
  clear_children(document.body);

  const text = document.createTextNode("abcdef");
  document.body.appendChild(text);

  const range = document.createRange();
  range.setStart(text, 0);
  range.setEnd(text, 3);

  text.data = "z";

  // startOffset=0 stays 0; endOffset>0 moves to 0.
  assert_equals(range.startOffset, 0, "startOffset=0 should remain 0 after replace data");
  assert_equals(range.endOffset, 0, "endOffset inside replaced range should move to 0");
  assert_true(range.collapsed, "range should be collapsed after data replacement");
}, "CharacterData.data replacement updates endOffset even when startOffset is 0");

