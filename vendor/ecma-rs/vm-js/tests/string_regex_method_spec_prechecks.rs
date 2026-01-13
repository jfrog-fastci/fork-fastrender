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

      function isTypeError(thunk) {
        try {
          thunk();
          return false;
        } catch (e) {
          return e && e.name === "TypeError";
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

      // Symbol primitives will still throw (ToString(Symbol) => TypeError), but they must not
      // consult `Symbol.prototype[Symbol.*]` during @@dispatch checks.
      poison(Symbol.prototype, Symbol.match);
      poison(Symbol.prototype, Symbol.search);
      poison(Symbol.prototype, Symbol.replace);
      poison(Symbol.prototype, Symbol.split);
      const symbol_ok = [
        isTypeError(() => "abc".match(Symbol("a"))),
        isTypeError(() => "abc".search(Symbol("a"))),
        isTypeError(() => "abc".replace(Symbol("a"), "x")),
        isTypeError(() => "abc".replaceAll(Symbol("a"), "x")),
        isTypeError(() => "abc".split(Symbol("a"))),
      ].every(Boolean);

      match_ok && search_ok && replace_ok && replace_all_ok && split_ok && symbol_ok;
    "#,
    Budget::unlimited(1),
    None,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn string_replace_and_replace_all_dispatch_before_tostring_receiver_and_use_original_receiver(
) -> Result<(), VmError> {
  let mut agent = new_agent();

  let value = agent.run_script(
    "string_replace_dispatch_before_tostring.js",
    r#"
      // If the builtins call `ToString(this)` before `@@replace` dispatch, `ToString(Symbol)` would
      // throw a TypeError and we would not get our custom result.

      const receiver = Symbol("replaceReceiver");
      const searchReplace = {
        [Symbol.replace](o, replaceValue) {
          if (this !== searchReplace) throw new Error("wrong this for @@replace");
          if (o !== receiver) throw new Error("wrong receiver passed to @@replace");
          if (replaceValue !== "x") throw new Error("wrong replaceValue passed to @@replace");
          return "okReplace";
        }
      };

      const receiver2 = Symbol("replaceAllReceiver");
      const searchReplaceAll = {
        [Symbol.replace](o, replaceValue) {
          if (this !== searchReplaceAll) throw new Error("wrong this for @@replace (replaceAll)");
          if (o !== receiver2) throw new Error("wrong receiver passed to @@replace (replaceAll)");
          if (replaceValue !== "y") throw new Error("wrong replaceValue passed to @@replace (replaceAll)");
          return "okReplaceAll";
        }
      };

      String.prototype.replace.call(receiver, searchReplace, "x") === "okReplace" &&
        String.prototype.replaceAll.call(receiver2, searchReplaceAll, "y") === "okReplaceAll";
    "#,
    Budget::unlimited(1),
    None,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

#[test]
fn string_search_string_methods_do_not_box_primitives_for_is_regexp() -> Result<(), VmError> {
  let mut agent = new_agent();

  let value = agent.run_script(
    "string_search_string_methods_do_not_box_primitives_for_is_regexp.js",
    r#"
      function thrower() { throw 123; }

      // IsRegExp(argument) must return false for primitives *without* boxing them and consulting
      // `@@match` on their prototypes.
      Object.defineProperty(Boolean.prototype, Symbol.match, { configurable: true, get: thrower });
      Object.defineProperty(Number.prototype,  Symbol.match, { configurable: true, get: thrower });
      Object.defineProperty(String.prototype,  Symbol.match, { configurable: true, get: thrower });
      Object.defineProperty(BigInt.prototype,  Symbol.match, { configurable: true, get: thrower });

      // These must not throw.
      "abc".includes(true);
      "abc".includes(1);
      "abc".includes("a");
      "a1b".includes(1n);

      "abc".startsWith(true);
      "abc".startsWith(1);
      "abc".startsWith("a");
      "a1b".startsWith(1n);

      "abc".endsWith(true);
      "abc".endsWith(1);
      "abc".endsWith("c");
      "a1b".endsWith(1n);

      // Symbol primitives still throw (ToString(Symbol) => TypeError), but must not consult
      // `Symbol.prototype[Symbol.match]` during IsRegExp checks.
      Object.defineProperty(Symbol.prototype, Symbol.match, { configurable: true, get: thrower });

      function isTypeError(thunk) {
        try { thunk(); } catch (e) { return e instanceof TypeError; }
        return false;
      }

      isTypeError(() => "abc".includes(Symbol("a"))) &&
        isTypeError(() => "abc".startsWith(Symbol("a"))) &&
        isTypeError(() => "abc".endsWith(Symbol("a")));
    "#,
    Budget::unlimited(1),
    None,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}
