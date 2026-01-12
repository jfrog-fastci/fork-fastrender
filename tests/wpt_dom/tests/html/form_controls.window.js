// META: script=/resources/testharness.js

test(() => {
  const input = document.createElement("input");
  assert_true(input instanceof HTMLInputElement);

  assert_equals(input.value, "");
  input.value = "hello";
  assert_equals(input.value, "hello");
}, "HTMLInputElement exists and exposes value");

test(() => {
  const input = document.createElement("input");

  assert_false(input.checked, "checked should default to false");
  input.checked = true;
  assert_true(input.checked);
  assert_true(input.getAttribute("checked") !== null);
  input.checked = false;
  assert_false(input.checked);
  assert_equals(input.getAttribute("checked"), null);

  assert_false(input.disabled, "disabled should default to false");
  input.disabled = true;
  assert_true(input.disabled);
  assert_true(input.getAttribute("disabled") !== null);
  input.disabled = false;
  assert_false(input.disabled);
  assert_equals(input.getAttribute("disabled"), null);
}, "HTMLInputElement checked/disabled reflect boolean attributes");

test(() => {
  const ta = document.createElement("textarea");
  assert_true(ta instanceof HTMLTextAreaElement);

  assert_equals(ta.value, "");
  ta.value = "hello";
  assert_equals(ta.value, "hello");
  assert_equals(ta.textContent, "hello");
}, "HTMLTextAreaElement exists and value reflects to textContent");

test(() => {
  const sel = document.createElement("select");
  assert_true(sel instanceof HTMLSelectElement);

  const options = sel.options;
  assert_equals(options.length, 0);

  const opt0 = document.createElement("option");
  opt0.setAttribute("value", "a");
  opt0.textContent = "A";
  sel.appendChild(opt0);
  assert_equals(options.length, 1);

  const opt1 = document.createElement("option");
  opt1.setAttribute("value", "b");
  opt1.textContent = "B";
  sel.appendChild(opt1);
  assert_equals(options.length, 2);

  assert_equals(sel.selectedIndex, 0);
  assert_equals(sel.value, "a");

  sel.selectedIndex = 1;
  assert_equals(sel.value, "b");

  sel.value = "a";
  assert_equals(sel.selectedIndex, 0);

  sel.value = "b";
  assert_equals(sel.selectedIndex, 1);
}, "HTMLSelectElement options/selectedIndex/value basics");

test(() => {
  const form = document.createElement("form");

  const input = document.createElement("input");
  const sel = document.createElement("select");
  const ta = document.createElement("textarea");

  const elements = form.elements;
  assert_equals(elements.length, 0);

  form.appendChild(input);
  assert_equals(elements.length, 1);
  assert_equals(elements[0], input);

  form.appendChild(sel);
  assert_equals(elements.length, 2);
  assert_equals(elements[1], sel);

  form.appendChild(ta);
  assert_equals(elements.length, 3);
  assert_equals(elements[2], ta);

  assert_equals(typeof form.submit, "function");
  try {
    form.submit();
  } catch (e) {
    assert_unreached("form.submit should be callable");
  }

  assert_equals(typeof form.reset, "function");
  try {
    form.reset();
  } catch (e) {
    assert_unreached("form.reset should be callable");
  }
}, "HTMLFormElement.elements is live and submit/reset are callable");

