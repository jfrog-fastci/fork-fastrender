test(() => {
  assert_equals(window.origin, "https://web-platform.test");
}, "window.origin is the serialized origin of the document URL");

test(() => {
  assert_equals(window.isSecureContext, true);
}, "window.isSecureContext is true for https:");

test(() => {
  assert_equals(window.crossOriginIsolated, false);
}, "window.crossOriginIsolated defaults to false");

test(() => {
  assert_equals(btoa("a"), "YQ==");
  assert_equals(atob("YQ=="), "a");
}, "btoa/atob roundtrip basic ASCII");

test(() => {
  assert_equals(atob(" Y Q = =\n"), "a");
}, "atob accepts ASCII whitespace (forgiving-base64 decode)");

test(() => {
  assert_equals(atob("YQ"), "a");
}, "atob accepts missing padding");

test(() => {
  let threw = false;
  try {
    atob("!!!");
  } catch (e) {
    threw = true;
    assert_equals(e.name, "InvalidCharacterError");
    assert_true(typeof e.message === "string");
  }
  assert_true(threw);
}, "atob invalid input throws InvalidCharacterError");

test(() => {
  let threw = false;
  try {
    btoa("Ā"); // U+0100 > 0xFF
  } catch (e) {
    threw = true;
    assert_equals(e.name, "InvalidCharacterError");
    assert_true(typeof e.message === "string");
  }
  assert_true(threw);
}, "btoa rejects code points outside Latin-1 and throws InvalidCharacterError");

test(() => {
  // Decodes bytes 0,1,2,3.
  const s = atob("AAECAw==");
  assert_equals(s.length, 4);
  assert_equals(s.charCodeAt(0), 0);
  assert_equals(s.charCodeAt(1), 1);
  assert_equals(s.charCodeAt(2), 2);
  assert_equals(s.charCodeAt(3), 3);
}, "atob returns a ByteString-like DOMString (char codes 0..255)");

test(() => {
  // `reportError` should never throw, even for values that stringify oddly.
  reportError(new Error("boom"));
  reportError(null);
  reportError(undefined);
  reportError({ foo: 1 });
  reportError(Symbol("x"));
}, "reportError does not throw");

