// META: script=/resources/testharness.js
//
// Live Range update behavior for CharacterData "replace data" operations.
//
// Setting `CharacterData.data` performs a "replace data" with offset=0 and count=oldLength.
// Per the DOM Standard, this updates live ranges whose start/end offsets are within the replaced
// region by clamping them to the replacement offset (0).
//
// Spec: https://dom.spec.whatwg.org/#concept-cd-replace

function clear_children(node) {
  while (node.childNodes.length !== 0) {
    node.removeChild(node.childNodes[0]);
  }
}

test(() => {
  clear_children(document.body);

  const host = document.createElement("div");
  document.body.appendChild(host);

  const t = document.createTextNode("abcd");
  host.appendChild(t);

  const r = document.createRange();
  r.setStart(t, 1);
  r.setEnd(t, 3);
  assert_equals(r.startOffset, 1);
  assert_equals(r.endOffset, 3);

  // CharacterData.data setter runs "replace data" with offset=0 and count=oldLength (=4).
  t.data = "wxyz";
  assert_equals(t.data, "wxyz");

  assert_equals(r.startContainer, t);
  assert_equals(r.endContainer, t);
  assert_equals(r.startOffset, 0);
  assert_equals(r.endOffset, 0);
}, "Live Range offsets reset to 0 when CharacterData.data replaces all data");
