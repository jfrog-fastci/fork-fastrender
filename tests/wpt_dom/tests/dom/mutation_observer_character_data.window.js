// META: script=/resources/testharness.js

promise_test(async () => {
  // Note: this harness supports at most one async test per file (see `resources/testharness.js`),
  // so keep all MutationObserver assertions inside a single `promise_test`.

  const c1 = document.createComment("a");
  const records1 = [];
  const mo1 = new MutationObserver((rs) => records1.push(...rs));
  mo1.observe(c1, { characterData: true, characterDataOldValue: true });

  c1.data = "b";
  await Promise.resolve();

  assert_equals(records1.length, 1);
  assert_equals(records1[0].type, "characterData");
  assert_equals(records1[0].target, c1);
  assert_equals(records1[0].oldValue, "a");
  mo1.disconnect();

  const t1 = document.createTextNode("a");
  const records2 = [];
  const mo2 = new MutationObserver((rs) => records2.push(...rs));
  mo2.observe(t1, { characterData: true, characterDataOldValue: true });

  t1.nodeValue = null;
  await Promise.resolve();

  assert_equals(t1.data, "");
  assert_equals(records2.length, 1);
  assert_equals(records2[0].type, "characterData");
  assert_equals(records2[0].target, t1);
  assert_equals(records2[0].oldValue, "a");
  mo2.disconnect();

  const c2 = document.createComment("a");
  const records3 = [];
  const mo3 = new MutationObserver((rs) => records3.push(...rs));
  mo3.observe(c2, { characterData: true });

  // No-op write must not queue records.
  c2.data = "a";
  await Promise.resolve();

  assert_equals(records3.length, 0);
  mo3.disconnect();
}, "MutationObserver characterData records for Comment.data/Text.nodeValue and not for no-op writes");
