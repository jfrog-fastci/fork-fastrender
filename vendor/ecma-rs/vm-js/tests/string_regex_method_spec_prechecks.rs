use vm_js::{Agent, Budget, HeapLimits, Value, VmError, VmOptions};

fn new_agent() -> Agent {
  Agent::with_options(
    VmOptions::default(),
    // Plenty for these small scripts, but large enough that we won't accidentally trip GC paths
    // that could obscure the ordering we want to test.
    HeapLimits::new(1024 * 1024, 1024 * 1024),
  )
  .expect("create agent")
}

#[test]
fn string_regex_methods_require_object_coercible() -> Result<(), VmError> {
  let mut agent = new_agent();

  let value = agent.run_script(
    "string_regex_methods_require_object_coercible.js",
    r#"
      function isTypeError(thunk) {
        try {
          thunk();
          return false;
        } catch (e) {
          return e && e.name === "TypeError";
        }
      }

      [
        isTypeError(() => String.prototype.match.call(null, /a/)),
        isTypeError(() => String.prototype.search.call(null, /a/)),
        isTypeError(() => String.prototype.replace.call(null, "a", "b")),
        isTypeError(() => String.prototype.replaceAll.call(null, "a", "b")),
        isTypeError(() => String.prototype.split.call(null, ",")),

        isTypeError(() => String.prototype.match.call(undefined, /a/)),
        isTypeError(() => String.prototype.search.call(undefined, /a/)),
        isTypeError(() => String.prototype.replace.call(undefined, "a", "b")),
        isTypeError(() => String.prototype.replaceAll.call(undefined, "a", "b")),
        isTypeError(() => String.prototype.split.call(undefined, ",")),
      ].every(Boolean);
    "#,
    Budget::unlimited(1),
    None,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn string_regex_methods_do_not_consult_symbol_methods_on_primitive_arguments() -> Result<(), VmError> {
  let mut agent = new_agent();

  let value = agent.run_script(
    "string_regex_methods_no_symbol_lookup_on_primitives.js",
    r#"
      function thrower() {
        throw new Error("unexpected @@* lookup on primitive");
      }

      function poison(proto, sym) {
        Object.defineProperty(proto, sym, { configurable: true, get: thrower });
      }

      function noThrow(thunk) {
        try {
          thunk();
          return true;
        } catch (e) {
          return false;
        }
      }

       // @@match
       poison(Boolean.prototype, Symbol.match);
       poison(Number.prototype, Symbol.match);
       poison(String.prototype, Symbol.match);
       poison(BigInt.prototype, Symbol.match);
       const match_ok = [
         noThrow(() => "abc".match(true)),
         noThrow(() => "abc".match(1)),
         noThrow(() => "abc".match("a")),
         noThrow(() => "abc".match(1n)),
       ].every(Boolean);

       // @@search
       poison(Boolean.prototype, Symbol.search);
       poison(Number.prototype, Symbol.search);
       poison(String.prototype, Symbol.search);
       poison(BigInt.prototype, Symbol.search);
       const search_ok = [
         noThrow(() => "abc".search(true)),
         noThrow(() => "abc".search(1)),
         noThrow(() => "abc".search("a")),
         noThrow(() => "abc".search(1n)),
       ].every(Boolean);

       // @@replace (both replace + replaceAll consult @@replace)
       poison(Boolean.prototype, Symbol.replace);
       poison(Number.prototype, Symbol.replace);
       poison(String.prototype, Symbol.replace);
       poison(BigInt.prototype, Symbol.replace);
       const replace_ok = [
         noThrow(() => "abc".replace(true, "x")),
         noThrow(() => "abc".replace(1, "x")),
         noThrow(() => "abc".replace("a", "x")),
         noThrow(() => "abc".replace(1n, "x")),
       ].every(Boolean);

       const replace_all_ok = [
         noThrow(() => "abc".replaceAll(true, "x")),
         noThrow(() => "abc".replaceAll(1, "x")),
         noThrow(() => "abc".replaceAll("a", "x")),
         noThrow(() => "abc".replaceAll(1n, "x")),
       ].every(Boolean);

       // @@split
       poison(Boolean.prototype, Symbol.split);
       poison(Number.prototype, Symbol.split);
       poison(String.prototype, Symbol.split);
       poison(BigInt.prototype, Symbol.split);
       const split_ok = [
         noThrow(() => "abc".split(true)),
         noThrow(() => "abc".split(1)),
         noThrow(() => "abc".split("a")),
         noThrow(() => "abc".split(1n)),
       ].every(Boolean);

      match_ok && search_ok && replace_ok && replace_all_ok && split_ok;
    "#,
    Budget::unlimited(1),
    None,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}
