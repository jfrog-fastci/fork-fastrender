// META: script=/resources/testharness.js
//
// Live Range updates for Text.splitText().
//
// This file is conditional: only run the tests when splitText is implemented.
//
// https://dom.spec.whatwg.org/#concept-text-split

function clear_children(node) {
  while (node.childNodes.length !== 0) {
    node.removeChild(node.childNodes[0]);
  }
}

if (typeof Text === "undefined" || typeof Text.prototype.splitText !== "function") {
  test(() => {
    assert_true(true);
  }, "Text.splitText is not implemented (skipping Range splitText live update tests)");
} else {
  test(() => {
    clear_children(document.body);

    const host = document.createElement("div");
    const text = document.createTextNode("hello");
    host.appendChild(text);
    document.body.appendChild(host);

    const range = document.createRange();
    range.setStart(text, 1);
    range.setEnd(text, 4);

    const new_text = text.splitText(2);

    assert_equals(text.data, "he", "original text node data should be truncated");
    assert_equals(new_text.data, "llo", "new text node data should be the trailing substring");

    // startOffset=1 <= split offset, so start stays in the original node.
    // endOffset=4 > split offset, so end moves to the new node and is reduced by the split offset.
    assert_equals(range.startContainer, text, "startContainer should remain in the original text node");
    assert_equals(range.startOffset, 1, "startOffset should not change when <= split offset");
    assert_equals(range.endContainer, new_text, "endContainer should move to the new text node");
    assert_equals(range.endOffset, 2, "endOffset should be reduced by the split offset");
  }, "Text.splitText updates live range boundary points that were inside the split text node");

  test(() => {
    clear_children(document.body);

    const host = document.createElement("div");
    const text = document.createTextNode("hello");
    const after = document.createElement("span");
    host.appendChild(text);
    host.appendChild(after);
    document.body.appendChild(host);

    // Boundary point is immediately after the text node, expressed in the parent.
    const range = document.createRange();
    range.setStart(host, 1);
    range.setEnd(host, 1);

    text.splitText(2);

    // After split, a new text node is inserted after `text`. Per the spec, live ranges whose
    // boundary point was `(parent, index(text) + 1)` are shifted forward by 1 so they stay after the
    // inserted node.
    assert_equals(range.startContainer, host, "startContainer should remain the parent");
    assert_equals(range.startOffset, 2, "startOffset should increase to remain after the inserted text node");
    assert_equals(range.endContainer, host, "endContainer should remain the parent");
    assert_equals(range.endOffset, 2, "endOffset should increase to remain after the inserted text node");
    assert_true(range.collapsed, "range should remain collapsed");
  }, "Text.splitText updates live range boundary points that were in the parent after the split node");
}

