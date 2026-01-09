function document_head_body_shims() {
  assert_equals(document.head.tagName, "HEAD");
  assert_equals(document.body.tagName, "BODY");

  const child = document.createElement("div");
  const returned = document.body.appendChild(child);
  assert_equals(returned, child, "appendChild should return the inserted node");
  assert_equals(document.body.childNodes.length, 1, "appendChild should record child");
  assert_equals(document.body.childNodes[0], child, "childNodes[0] should be the inserted node");
}

test(document_head_body_shims, "document.head/body shims");
