use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn global_parse_int_radix_edge_cases() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var ok = true;

      // ToInt32(radix) wrapping: 2^32 -> 0 -> default radix path.
      ok = ok && parseInt("10", 4294967296) === 10;

      // Range check.
      ok = ok && isNaN(parseInt("10", 37));

      // Prefix stripping rules.
      ok = ok && parseInt("0x10") === 16;
      ok = ok && parseInt("0x10", 0) === 16;
      ok = ok && parseInt("0x10", 16) === 16;
      ok = ok && parseInt("0x10", 10) === 0;

      // Preserve -0.
      ok = ok && (1 / parseInt("-0")) === -1e999;

      ok;
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn global_parse_float_prefix_and_infinity() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var ok = true;

      ok = ok && parseFloat("  +Infinityxyz") === 1e999;
      ok = ok && parseFloat("1.5px") === 1.5;
      ok = ok && parseFloat(".5") === 0.5;
      ok = ok && parseFloat("0x10") === 0;

      // Exponent forms: incomplete exponent stops before the `e`.
      ok = ok && parseFloat("1e") === 1;
      ok = ok && parseFloat("1e+") === 1;
      ok = ok && parseFloat("1e-") === 1;

      ok = ok && isNaN(parseFloat("x"));

      ok;
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn global_uri_encode_decode_and_errors() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var ok = true;

      ok = ok && encodeURIComponent("a b") === "a%20b";

      // encodeURI preserves reserved characters.
      ok = ok && encodeURI("http://example.com/a?b=c#d") === "http://example.com/a?b=c#d";

      // encodeURIComponent escapes reserved characters (but leaves '.' unescaped).
      ok = ok && encodeURIComponent("http://example.com/a?b=c#d") === "http%3A%2F%2Fexample.com%2Fa%3Fb%3Dc%23d";

      ok = ok && decodeURIComponent("a%20b") === "a b";

      // decodeURI preserves escape sequences for reserved characters, decodeURIComponent decodes them.
      ok = ok && decodeURI("%3B") === "%3B";
      ok = ok && decodeURIComponent("%3B") === ";";

      // Roundtrip some non-ASCII.
      ok = ok && decodeURIComponent(encodeURIComponent("✓")) === "✓";

      // Malformed percent-encoding should throw URIError.
      var threw = false;
      try { decodeURIComponent("%E0%A4"); } catch (e) { threw = e.name === "URIError"; }
      ok = ok && threw;

      // RFC 3629 invalid UTF-8 sequence (overlong encoding) should throw.
      threw = false;
      try { decodeURIComponent("%C0%80"); } catch (e) { threw = e.name === "URIError"; }
      ok = ok && threw;

      // encodeURI must throw on unpaired surrogates.
      threw = false;
      try { encodeURI("\uD800"); } catch (e) { threw = e.name === "URIError"; }
      ok = ok && threw;

      ok;
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

