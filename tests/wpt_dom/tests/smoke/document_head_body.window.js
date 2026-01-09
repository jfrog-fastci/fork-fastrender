test(() => {
  assert_true(!!document.head, "document.head should exist");
  assert_true(!!document.body, "document.body should exist");

  // Case-insensitive tagName matching is acceptable.
  assert_equals(String(document.head.tagName).toUpperCase(), "HEAD");
  assert_equals(String(document.body.tagName).toUpperCase(), "BODY");

  const child = document.createElement("div");
  const returned = document.body.appendChild(child);
  assert_equals(returned, child, "appendChild should return the inserted node");
  assert_equals(document.body.childNodes.length, 1, "appendChild should record child");
  assert_equals(document.body.childNodes[0], child, "childNodes[0] should be the inserted node");
}, "document.head/body shims");

