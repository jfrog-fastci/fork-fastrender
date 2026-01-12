// META: script=/resources/testharness.js

test(() => {
  const input = document.createElement("input");

  assert_equals(input.value, "");

  // The `value` IDL attribute is "spec-ish": attribute mutations update `value` until the dirty
  // value flag is set by script writes.
  input.setAttribute("value", "a");
  assert_equals(input.value, "a", "value reflects the content attribute before becoming dirty");

  input.value = "b";
  assert_equals(input.value, "b", "setting value updates the IDL attribute");
  assert_equals(
    input.getAttribute("value"),
    "a",
    "setting the IDL value does not mutate the value content attribute"
  );

  input.setAttribute("value", "c");
  assert_equals(
    input.value,
    "b",
    "after becoming dirty, content attribute mutations do not affect the current value"
  );
}, "HTMLInputElement.value dirty value flag behavior");

test(() => {
  const input = document.createElement("input");
  input.type = "checkbox";

  assert_false(input.checked);
  assert_false(input.hasAttribute("checked"));

  input.checked = true;
  assert_true(input.checked);
  assert_false(input.hasAttribute("checked"), "checked does not reflect to the checked attribute");
}, "HTMLInputElement.checked does not reflect to the checked attribute");

test(() => {
  const input = document.createElement("input");
  input.type = "checkbox";

  input.setAttribute("checked", "");
  assert_true(input.checked);

  input.checked = false;
  assert_false(input.checked);
  assert_true(
    input.hasAttribute("checked"),
    "setting checked does not remove the checked content attribute"
  );

  // Once the checkedness is dirty, later attribute mutations should not clobber the state.
  input.removeAttribute("checked");
  input.setAttribute("checked", "");
  assert_false(input.checked);
}, "HTMLInputElement.checked dirty checkedness flag behavior");

test(() => {
  const input = document.createElement("input");

  assert_false(input.disabled);
  assert_false(input.hasAttribute("disabled"));
  input.disabled = true;
  assert_true(input.disabled);
  assert_true(input.hasAttribute("disabled"));
  input.disabled = false;
  assert_false(input.disabled);
  assert_false(input.hasAttribute("disabled"));
}, "HTMLInputElement.disabled reflects to the disabled attribute");

test(() => {
  const ta = document.createElement("textarea");
  assert_equals(ta.value, "");
  assert_equals(ta.textContent, "");

  ta.textContent = "hello";
  assert_equals(ta.value, "hello", "value reflects default textContent before becoming dirty");

  ta.value = "world";
  assert_equals(ta.value, "world");
  assert_equals(
    ta.textContent,
    "hello",
    "setting value does not mutate the element's default textContent"
  );

  ta.textContent = "changed";
  assert_equals(ta.value, "world", "dirty value flag preserves the value against textContent changes");
}, "HTMLTextAreaElement.value dirty value flag behavior");

test(() => {
  const sel = document.createElement("select");
  assert_equals(sel.options.length, 0);
  assert_equals(sel.selectedIndex, -1);
  assert_equals(sel.value, "");

  const opt0 = document.createElement("option");
  opt0.textContent = "A";
  sel.appendChild(opt0);

  const opt1 = document.createElement("option");
  opt1.setAttribute("value", "b");
  opt1.textContent = "B";
  sel.appendChild(opt1);

  assert_equals(sel.options.length, 2);
  assert_equals(sel.options[0], opt0);
  assert_equals(sel.options[1], opt1);

  assert_equals(sel.selectedIndex, 0);
  assert_equals(sel.value, "A", "option value falls back to textContent when value attribute is missing");

  sel.selectedIndex = 1;
  assert_equals(sel.value, "b");

  sel.value = "A";
  assert_equals(sel.selectedIndex, 0);

  sel.value = "b";
  assert_equals(sel.selectedIndex, 1);

  sel.value = "nope";
  assert_equals(sel.selectedIndex, 1, "non-matching value does not change selection");
  assert_equals(sel.value, "b", "non-matching value does not change value");
}, "HTMLSelectElement options/selectedIndex/value basics");

test(() => {
  const form = document.createElement("form");
  const input = document.createElement("input");
  const sel = document.createElement("select");
  const ta = document.createElement("textarea");

  assert_equals(form.elements.length, 0);

  form.appendChild(input);
  assert_equals(form.elements.length, 1);
  assert_equals(form.elements[0], input);

  form.appendChild(sel);
  assert_equals(form.elements.length, 2);
  assert_equals(form.elements[1], sel);

  form.appendChild(ta);
  assert_equals(form.elements.length, 3);
  assert_equals(form.elements[2], ta);

  assert_equals(typeof form.submit, "function");
  assert_equals(typeof form.reset, "function");
  form.submit();
  form.reset();
}, "HTMLFormElement.elements/submit/reset basic shape");
