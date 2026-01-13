// META: script=/resources/testharness.js

test(() => {
  assert_equals(typeof CharacterData, "function");
  assert_equals(typeof CharacterData.prototype.deleteData, "function");
}, "CharacterData.prototype.deleteData is exposed");

test(() => {
  const t = document.createTextNode("a\u{1F4A9}b"); // "a💩b" (emoji is 2 UTF-16 code units)
  assert_true(t instanceof CharacterData);
  t.deleteData(1, 2);
  assert_equals(t.data, "ab");
}, "Text.deleteData deletes UTF-16 code units (surrogate pairs count as 2)");

test(() => {
  const c = document.createComment("a\u{1F4A9}b");
  assert_true(c instanceof CharacterData);
  c.deleteData(1, 2);
  assert_equals(c.data, "ab");
}, "Comment.deleteData deletes UTF-16 code units");

test(() => {
  const pi = document.createProcessingInstruction("x", "a\u{1F4A9}b");
  assert_true(pi instanceof CharacterData);
  pi.deleteData(1, 2);
  assert_equals(pi.data, "ab");
}, "ProcessingInstruction.deleteData deletes UTF-16 code units");

test(() => {
  const t = document.createTextNode("abc");
  assert_throws_dom("IndexSizeError", () => t.deleteData(4, 1));
}, "CharacterData.deleteData throws IndexSizeError when offset > length");
