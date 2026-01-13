use vm_js::{
  Budget, Heap, HeapLimits, JsRuntime, PropertyDescriptor, PropertyKey, PropertyKind,
  TerminationReason, Value, Vm, VmError, VmOptions,
};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn as_utf8_lossy(rt: &JsRuntime, value: Value) -> String {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  rt.heap().get_string(s).unwrap().to_utf8_lossy()
}

fn define_global(rt: &mut JsRuntime, key: &str, value: Value) {
  let global = rt.realm().global_object();
  let mut scope = rt.heap_mut().scope();
  scope.push_roots(&[Value::Object(global), value]).unwrap();
  let key_s = scope.alloc_string(key).unwrap();
  scope.push_root(Value::String(key_s)).unwrap();
  let desc = PropertyDescriptor {
    enumerable: true,
    configurable: true,
    kind: PropertyKind::Data { value, writable: true },
  };
  scope
    .define_property(global, PropertyKey::from_string(key_s), desc)
    .unwrap();
}

#[test]
fn regexp_flags_parsing_rejects_duplicates() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"try { new RegExp("a", "gg"); "no"; } catch (e) { e.name }"#)
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "SyntaxError");
}

#[test]
fn regexp_flags_parsing_accepts_v() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"new RegExp(".", "v").flags"#).unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "v");
}

#[test]
fn regexp_flags_parsing_rejects_uv_combination() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"try { new RegExp(".", "uv"); "no"; } catch (e) { e.name }"#)
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "SyntaxError");
}

#[test]
fn regexp_unicode_mode_rejects_standalone_right_bracket() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"try { new RegExp("]", "u"); "no"; } catch (e) { e.name }"#)
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "SyntaxError");
}

#[test]
fn regexp_literal_with_uv_flags_is_early_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script(r#"var r = /./uv;"#).unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
}

#[test]
fn regexp_unicode_sets_class_set_character_non_bmp_literal() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var r = new RegExp("[👨]", "v");
        [r.test("👨"), r.test("\uD83D")].join(",")
      "#,
    )
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "true,false");
}

#[test]
fn regexp_unicode_sets_family_zwj_matches_first_code_point() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        "👨‍👩‍👧‍👦".match(new RegExp("[👨‍👩‍👧‍👦]", "v"))[0]
      "#,
    )
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "👨");
}

#[test]
fn regexp_invalid_character_class_range_throws_syntax_error() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"try { new RegExp("[z-a]"); "no"; } catch (e) { e.name }"#)
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "SyntaxError");
}

#[test]
fn regexp_invalid_range_ordering_throws_syntax_error() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"try { new RegExp("[d-G\\c0001]"); "no"; } catch (e) { e.name }"#)
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "SyntaxError");

  let value = rt
    .exec_script(r#"try { new RegExp("[\\c0001d-G]"); "no"; } catch (e) { e.name }"#)
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "SyntaxError");
}

#[test]
fn regexp_valid_character_class_range_still_matches() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var r = new RegExp("[a-c]");
        "" + r.test("b")
      "#,
    )
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "true");
}

#[test]
fn regexp_prototype_unicode_accessor_basics() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var desc = Object.getOwnPropertyDescriptor(RegExp.prototype, "unicode");
        var ok =
          desc !== undefined &&
          desc.enumerable === false &&
          desc.configurable === true &&
          typeof desc.get === "function" &&
          desc.set === undefined &&
          desc.get.name === "get unicode" &&
          desc.get.length === 0 &&
          /a/u.unicode === true &&
          /a/.unicode === false &&
          (function () { try { desc.get.call(1); return false; } catch (e) { return e.name === "TypeError"; } })() &&
          (function () { try { return desc.get.call(RegExp.prototype) === undefined; } catch (e) { return false; } })();
        ok
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn regexp_prototype_source_escape_regexp_pattern() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        [
          new RegExp("a/b").source,
          /a\//.source,
          RegExp("a", "").source,
        ].join("|")
      "#,
    )
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), r#"a\/b|a\/|a"#);
}

#[test]
fn regexp_legacy_octal_escape_without_captures() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"new RegExp("\\1").exec("\u0001")[0]"#).unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "\u{1}");
}

#[test]
fn regexp_decimal_escape_backreference_takes_precedence() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"new RegExp("(.)\\1").exec("aa")[0]"#).unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "aa");
}

#[test]
fn regexp_class_legacy_octal_escape() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"new RegExp("[\\1]").exec("\u0001")[0]"#).unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "\u{1}");
}

#[test]
fn regexp_identity_escape_8_in_non_unicode_mode() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"String(new RegExp("\\8").test("8"))"#)
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "true");
}

#[test]
fn regexp_legacy_octal_escape_max_length_rule() {
  // `\400` parses as `\40` (octal for U+0020) followed by a literal `0`.
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"new RegExp("\\400").exec(" 0")[0]"#).unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), " 0");
}

#[test]
fn regexp_unicode_mode_rejects_invalid_numeric_escape_backreference() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"try { new RegExp("\\1", "u"); "no"; } catch (e) { e.name }"#)
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "SyntaxError");
}

#[test]
fn regexp_unicode_mode_allows_forward_numeric_escape_backreference() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"new RegExp("\\1(a)", "u").test("a")"#).unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn regexp_unicode_mode_rejects_legacy_octal_escape_sequence_00() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"try { new RegExp("\\00", "u"); "no"; } catch (e) { e.name }"#)
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "SyntaxError");
}

#[test]
fn regexp_unicode_mode_rejects_escape_8() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"try { new RegExp("\\8", "u"); "no"; } catch (e) { e.name }"#)
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "SyntaxError");
}

#[test]
fn regexp_prototype_flags_basic() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"/a/gim.flags"#).unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "gim");
}

#[test]
fn regexp_prototype_flags_get_order_is_observable() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var r = /a/;
        var calls = [];
        function def(name, marker) {
          Object.defineProperty(r, name, {
            get: function () { calls.push(marker); return 1; },
            configurable: true,
          });
        }

        def("hasIndices", "d");
        def("global", "g");
        def("ignoreCase", "i");
        def("multiline", "m");
        def("dotAll", "s");
        def("unicode", "u");
        def("unicodeSets", "v");
        def("sticky", "y");

        r.flags + "|" + calls.join("")
      "#,
    )
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "dgimsuvy|dgimsuvy");
}

#[test]
fn regexp_prototype_flags_get_rethrows() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var r = /a/;
        var calls = [];
        Object.defineProperty(r, "hasIndices", {
          get: function () { calls.push("d"); return 0; },
          configurable: true,
        });
        Object.defineProperty(r, "global", {
          get: function () { calls.push("g"); throw "boom"; },
          configurable: true,
        });
        Object.defineProperty(r, "ignoreCase", {
          get: function () { calls.push("i"); return 1; },
          configurable: true,
        });
        try {
          r.flags;
          "no";
        } catch (e) {
          e + "|" + calls.join("")
        }
      "#,
    )
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "boom|dg");
}

#[test]
fn regexp_unicode_sets_accessor() {
  let mut rt = new_runtime();

  let v = rt
    .exec_script(
      r#"
        var d = Object.getOwnPropertyDescriptor(RegExp.prototype, "unicodeSets");
        d !== undefined &&
        d.enumerable === false &&
        d.configurable === true &&
        typeof d.get === "function" &&
        d.get.name === "get unicodeSets" &&
        d.get.length === 0 &&
        d.set === undefined
      "#,
    )
    .unwrap();
  assert_eq!(v, Value::Bool(true));

  // `%RegExp.prototype%.unicodeSets` is defined and returns `undefined` on the prototype itself.
  let value = rt.exec_script(r#"RegExp.prototype.unicodeSets"#).unwrap();
  assert!(matches!(value, Value::Undefined));

  // RegExpHasFlag semantics for `v` (unicode sets).
  let value = rt.exec_script(r#"/./v.unicodeSets"#).unwrap();
  assert_eq!(value, Value::Bool(true));
  let value = rt.exec_script(r#"/./d.unicodeSets"#).unwrap();
  assert_eq!(value, Value::Bool(false));

  let value = rt
    .exec_script(
      r#"
        var get = Object.getOwnPropertyDescriptor(RegExp.prototype, "unicodeSets").get;
        function errName(v) {
          try { get.call(v); return "no"; } catch (e) { return e.name; }
        }
        [errName(undefined), errName(1), errName({})].join(",")
      "#,
    )
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "TypeError,TypeError,TypeError");

  // `RegExp.prototype.flags` includes `v` in the canonical flags string (including for literals).
  let value = rt.exec_script(r#"/./v.flags"#).unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "v");
}

#[test]
fn regexp_last_index_global_exec_updates_and_resets() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var r = /a/g;
        r.exec("a");
        var after_match = r.lastIndex;
        r.lastIndex = 1;
        r.exec("a");
        [after_match, r.lastIndex].join(",")
      "#,
    )
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "1,0");
}

#[test]
fn regexp_last_index_sticky_semantics() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var r = /a/y;
        r.lastIndex = 1;
        var ok1 = r.test("ba");
        var li1 = r.lastIndex;
        r.lastIndex = 0;
        var ok2 = r.test("ba");
        [ok1, li1, ok2, r.lastIndex].join(",")
      "#,
    )
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "true,2,false,0");
}

#[test]
fn regexp_unicode_mode_consumes_surrogate_pairs_for_dot() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var s = "💩";
        var ok_no_unicode = /^.$/.test(s);
        var ok_u = /^.$/u.test(s);
        var ok_v = new RegExp("^.$", "v").test(s);

        // Unpaired surrogates are still matched as single code points in Unicode mode.
        var lone_high = String.fromCharCode(0xD800);
        var lone_low = String.fromCharCode(0xDC00);
        var ok_lone_high = /^.$/u.test(lone_high);
        var ok_lone_low = /^.$/u.test(lone_low);

        [ok_no_unicode, ok_u, ok_v, ok_lone_high, ok_lone_low].join(",")
      "#,
    )
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "false,true,true,true,true");
}

#[test]
fn regexp_unicode_mode_parses_non_bmp_literals_and_classes_as_code_points() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var two = "💩💩";
        // In Unicode mode, the `{2}` quantifier applies to the whole code point.
        var ok_u = /^💩{2}$/u.test(two);
        var ok_v = new RegExp("^💩{2}$", "v").test(two);
        // Without Unicode mode, the quantifier applies to the second UTF-16 code unit only.
        var ok_no_unicode = /^💩{2}$/.test(two);

        // Character classes match code points under /u.
        var ok_class_literal = /^[💩]$/u.test("💩");
        var ok_class_escape_pair = /^[\uD83D\uDCA9]$/u.test("💩");

        // \u{...} escapes produce full code points in Unicode mode.
        var ok_u_brace_escape = new RegExp("^\\u{1F4A9}$", "u").test("💩");

        // \uXXXX escapes can pair in Unicode mode to form a supplementary code point.
        var ok_u_surrogate_pair_escape = new RegExp("^\\uD83D\\uDCA9$", "u").test("💩");

        [ok_u, ok_v, ok_no_unicode, ok_class_literal, ok_class_escape_pair, ok_u_brace_escape, ok_u_surrogate_pair_escape].join(",")
      "#,
    )
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "true,true,false,true,true,true,true");
}

#[test]
fn regexp_exec_reads_lastindex_even_without_global_or_sticky() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        // Mirror test262 `failure-lastindex-access.js` / `success-lastindex-access.js`:
        // when /g and /y are unset, RegExpBuiltinExec still reads `lastIndex` and performs ToLength,
        // but does not write it back.
        let gets = 0;
        let counter = {
          valueOf() { gets++; return 0; }
        };
        const r = /a/;
        r.lastIndex = counter;

        const fail = r.exec("nbc");
        const ok1 = fail === null && r.lastIndex === counter;
        const succ = r.exec("abc");
        const ok2 = succ !== null && succ[0] === "a" && r.lastIndex === counter;
        ok1 && ok2 && gets === 2
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn string_regex_methods_basic_match_search_replace_split() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        "aba".match(/a/g).join(",") + "|" +
        "aba".search(/b/) + "|" +
        "1a2".replace(/\d/g, "x") + "|" +
        "a,b;c".split(/[;,]/).join("|")
      "#,
    )
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "a,a|1|xax|a|b|c");
}

#[test]
fn string_match_primitive_does_not_consult_symbol_match() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        Object.defineProperty(Boolean.prototype, Symbol.match, {
          get() { throw new Error("should not access Boolean.prototype[Symbol.match]"); },
          configurable: true,
        });
        JSON.stringify("atrue".match(true))
      "#,
    )
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "[\"true\"]");
}

#[test]
fn regexp_match_indices_basic() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#""abc".match(/a/d).indices[0].join(",")"#)
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "0,1");
}

#[test]
fn regexp_match_indices_captures_and_unmatched() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var a = "abc".match(/(a)?b/d).indices[1].join(",");
        var b = "bc".match(/(a)?b/d).indices[1];
        a + "|" + (b === undefined)
      "#,
    )
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "0,1|true");
}

#[test]
fn regexp_match_indices_groups_duplicate_named_properties() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        const matcher = /(?:(?<x>a)|(?<y>a)(?<x>b))(?:(?<z>c)|(?<z>d))/d;
        const three = "abc".match(matcher);
        const out1 =
          three.indices.groups.x.join(",") + "|" +
          three.indices.groups.y.join(",") + "|" +
          three.indices.groups.z.join(",") + "|" +
          Object.keys(three.indices.groups).join(",");

        const two = "ad".match(matcher);
        const out2 =
          two.indices.groups.x.join(",") + "|" +
          (two.indices.groups.y === undefined) + "|" +
          two.indices.groups.z.join(",") + "|" +
          Object.keys(two.indices.groups).join(",");

        const iteratedMatcher = /(?:(?:(?<x>a)|(?<x>b)|c)\k<x>){2}/d;
        const prev = "aac".match(iteratedMatcher);
        const out3 = (prev.indices.groups.x === undefined);

        [out1, out2, out3].join(";")
      "#,
    )
    .unwrap();
  assert_eq!(
    as_utf8_lossy(&rt, value),
    "1,2|0,1|2,3|x,y,z;0,1|true|1,2|x,y,z;true"
  );
}

#[test]
fn regexp_s_and_s_match_full_ecma262_whitespace_and_lineterminators() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        [
          /(\s)/.test("\u2028"), // Line Separator
          /\S/.test("\u2028"),
          /\s/.test("\u2003"), // Em Space (Zs)
          /\S/.test("\u2003"),
        ].join(",")
      "#,
    )
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "true,false,true,false");
}

#[test]
fn string_replace_get_substitution_two_digit_fallback() {
  // Regression test for GetSubstitution `$nn` parsing: `$11` should be treated as `$1` + `1` when
  // capture 11 does not exist (ECMA-262 GetSubstitution step 5.f.vi).
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#""uid=31".replace(/(uid=)(\d+)/, "$11" + 15)"#)
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "uid=115");
}

#[test]
fn regexp_prototype_to_string_formats_source_and_flags() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"String(/./g)"#).unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "/./g");
}

#[test]
fn match_all_iterator_is_iterable() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var out = "";
        for (var m of "ab".matchAll(/a/g)) {
          out += m[0];
        }
        out
      "#,
    )
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "a");
}

#[test]
fn regexp_quantifier_iterations_reset_inner_captures() {
  // Regression test: capture groups inside quantified atoms must be cleared between iterations so
  // captures from earlier iterations don't leak when the final iteration doesn't participate.
  let mut rt = new_runtime();

  let value = rt
    .exec_script(r#""ab".match(/(?:(a)|b)+/)[1]"#)
    .unwrap();
  assert_eq!(value, Value::Undefined);

  // Same scenario but with a *lazy* quantifier; the engine must still clear capture slots for the
  // states that enter a new iteration body.
  let value = rt
    .exec_script(r#""ab".match(/^(?:(a)|b)+?$/)[1]"#)
    .unwrap();
  assert_eq!(value, Value::Undefined);

  // Backreference should observe the cleared capture from the last iteration.
  let value = rt
    .exec_script(
      r#"(() => {
        const m = "ab".match(/(?:(a)|b)+\1/);
        return m !== null && m[0] === "ab" && m[1] === undefined;
      })()"#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));

  // Nested quantifier: the inner optional `(b)?` capture must also be cleared between `{2}`
  // iterations.
  let value = rt
    .exec_script(r#""cba".match(/(?:(a)|c(b)?){2}/)[2]"#)
    .unwrap();
  assert_eq!(value, Value::Undefined);
}

#[test]
fn regexp_named_capture_groups_basic() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#""a".match(/(?<x>a)/).groups.x"#).unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "a");
}

#[test]
fn regexp_named_capture_groups_duplicate_names_and_order() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        const matcher = /(?:(?<x>a)|(?<y>a)(?<x>b))(?:(?<z>c)|(?<z>d))/;
        const three = "abc".match(matcher);
        const two = "ad".match(matcher);
        three.groups.x + "," + three.groups.y + "," + three.groups.z + "|" +
        Object.keys(three.groups).join(",") + "|" +
        two.groups.x + "," + String(two.groups.y) + "," + two.groups.z + "|" +
        Object.keys(two.groups).join(",")
      "#,
    )
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "b,a,c|x,y,z|a,undefined,d|x,y,z");
}

#[test]
fn regexp_named_backref_basic() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var r = /(?:(?<x>a)|(?<x>b)|c)\k<x>/;
        [r.test("aa"), r.test("bb"), r.test("c")].join(",")
      "#,
    )
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "true,true,true");
}

#[test]
fn regexp_named_groups_reset_in_quantifier_iterations() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        const r = /(?:(?:(?<x>a)|(?<x>b)|c)\k<x>){2}/;
        const m = "aac".match(r);
        String(m !== null && m.groups.x === undefined && Object.keys(m.groups).join(",") === "x")
      "#,
    )
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "true");
}

#[test]
fn regexp_unicode_property_escape_ascii_and_script_han() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        const text = "\u{20BB7}a\u{20BB7}";
        const m1u = new RegExp("\\p{Script=Han}", "u").exec(text);
        const m1v = new RegExp("\\p{Script=Han}", "v").exec(text);
        const m2u = new RegExp("\\p{ASCII}", "u").exec(text);
        const m2v = new RegExp("\\p{ASCII}", "v").exec(text);
        const m3u = new RegExp("\\P{ASCII}", "u").exec("a\u{20BB7}b");
        const m3v = new RegExp("\\P{ASCII}", "v").exec("a\u{20BB7}b");
        [m1u[0], m1u.index, m1v[0], m1v.index,
         m2u[0], m2u.index, m2v[0], m2v.index,
         m3u[0], m3u.index, m3v[0], m3v.index].join(",")
      "#,
    )
    .unwrap();
  assert_eq!(
    as_utf8_lossy(&rt, value),
    "𠮷,0,𠮷,0,a,2,a,2,𠮷,1,𠮷,1"
  );
}

#[test]
fn regexp_string_iterator_next_proxy_receiver_throws_type_error() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        try {
          const it = /a/g[Symbol.matchAll]("a");
          const p = new Proxy(it, {});
          p.next();
          "no";
        } catch (e) { e.name }
      "#,
    )
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "TypeError");
}

#[test]
fn regexp_engine_catastrophic_backtracking_is_interruptible() {
  let mut vm = Vm::new(VmOptions::default());
  vm.set_budget(Budget {
    fuel: Some(500),
    deadline: None,
    check_time_every: 1,
  });
  let heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap).unwrap();

  let err = rt
    .exec_script(r#"var r = /^(a+)+$/; r.test("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa!");"#)
    .unwrap_err();
  match err {
    VmError::Termination(term) => assert_eq!(term.reason, TerminationReason::OutOfFuel),
    other => panic!("expected termination, got {other:?}"),
  }
}

#[test]
fn regexp_engine_lookbehind_catastrophic_backtracking_is_interruptible() {
  let mut vm = Vm::new(VmOptions::default());
  vm.set_budget(Budget {
    fuel: Some(500),
    deadline: None,
    check_time_every: 1,
  });
  let heap = Heap::new(HeapLimits::new(8 * 1024 * 1024, 8 * 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap).unwrap();

  let err = rt
    .exec_script(r#"var r = /(?<=^(a+)+$)/; r.test("!aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");"#)
    .unwrap_err();
  match err {
    VmError::Termination(term) => assert_eq!(term.reason, TerminationReason::OutOfFuel),
    other => panic!("expected termination, got {other:?}"),
  }
}

#[test]
fn regexp_unicode_backreference_does_not_match_partial_surrogate_pair() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"/foo(.+)bar\1/u.exec("foo\uD834bar\uD834\uDC00")"#)
    .unwrap();
  assert!(matches!(value, Value::Null));
}

#[test]
fn regexp_unicode_backreference_does_not_match_surrogate_pair_partially() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"/(.+).*\1/u.test("\uD800\uDC00\uD800")"#)
    .unwrap();
  assert!(matches!(value, Value::Bool(false)));
}

#[test]
fn regexp_compilation_respects_heap_limits() {
  // The runtime itself needs some headroom; use the default 4MiB heap limit but feed a large
  // enough pattern that compilation would exceed it.
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap).unwrap();

  // Allocate a large pattern string on the GC heap; compilation should fail with `VmError::OutOfMemory`
  // *before* allocating large off-heap compilation buffers.
  let pattern = {
    let mut units: Vec<u16> = Vec::new();
    units.try_reserve_exact(200_000).unwrap();
    units.resize(200_000, b'a' as u16);
    let mut scope = rt.heap_mut().scope();
    scope.alloc_string_from_u16_vec(units).unwrap()
  };
  define_global(&mut rt, "P", Value::String(pattern));

  let err = rt.exec_script(r#"new RegExp(P)"#).unwrap_err();
  assert!(matches!(err, VmError::OutOfMemory));
}

#[test]
fn regexp_unicode_mode_escape_validation() {
  let mut rt = new_runtime();

  // UnicodeMode rejects invalid identity escapes (IdentityEscape[+UnicodeMode] is limited to
  // SyntaxCharacter or '/').
  let value = rt
    .exec_script(r#"try { new RegExp("\\M", "u"); "no"; } catch (e) { e.name }"#)
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "SyntaxError");

  // UnicodeMode rejects invalid control escapes (`\c` must be followed by an ASCII letter).
  let value = rt
    .exec_script(r#"try { new RegExp("\\c0", "u"); "no"; } catch (e) { e.name }"#)
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "SyntaxError");

  // UnicodeMode rejects invalid decimal escapes/backreferences.
  let value = rt
    .exec_script(r#"try { new RegExp("\\1", "u"); "no"; } catch (e) { e.name }"#)
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "SyntaxError");

  // Escaping a SyntaxCharacter is valid under UnicodeMode.
  let value = rt
    .exec_script(r#"try { new RegExp("\\{", "u"); "ok"; } catch (e) { e.name }"#)
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "ok");
}

#[test]
fn regexp_execution_backtracking_state_respects_heap_limits() {
  let mut vm = Vm::new(VmOptions::default());
  vm.set_budget(Budget {
    fuel: Some(200_000),
    deadline: None,
    check_time_every: 1,
  });
  let heap = Heap::new(HeapLimits::new(4 * 1024 * 1024, 4 * 1024 * 1024));
  let mut rt = JsRuntime::new(vm, heap).unwrap();

  // Build a pattern with a large number of `?` quantifiers. The backtracking VM stores one
  // continuation per quantifier; each backtracking state allocates a `repeats` vector whose length
  // is proportional to the total number of quantified atoms in the program. This makes the
  // backtracking stack memory usage attacker-controlled.
  const N: usize = 1000;
  let pattern_src: String = {
    let mut s = String::from("^");
    for _ in 0..N {
      s.push('a');
      s.push('?');
    }
    s.push('$');
    s
  };
  let input_src: String = {
    let mut s = String::new();
    s.reserve(N + 1);
    for _ in 0..N {
      s.push('a');
    }
    s.push('!');
    s
  };

  let mut scope = rt.heap_mut().scope();
  let pattern = scope.alloc_string(&pattern_src).unwrap();
  let input = scope.alloc_string(&input_src).unwrap();
  drop(scope);
  define_global(&mut rt, "P", Value::String(pattern));
  define_global(&mut rt, "S", Value::String(input));

  let err = rt
    .exec_script(
      r#"
        var r = new RegExp(P);
        r.test(S);
      "#,
    )
    .unwrap_err();

  match err {
    VmError::OutOfMemory => {}
    VmError::Termination(term) => assert_eq!(term.reason, TerminationReason::OutOfFuel),
    other => panic!("expected OOM/termination, got {other:?}"),
  }
}

#[test]
fn regexp_negated_empty_class_matches_any_code_unit() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#""a\nb".match(/[^]/g).length === 3"#).unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn regexp_negated_class_with_literal_closing_bracket_is_not_empty() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#""a]".match(/[^]]/g).join("") === "a""#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn regexp_character_class_whitespace_escape_matches_ecma_whitespace_and_line_terminators() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var ws = String.fromCharCode(
          0x0009, 0x000A, 0x000B, 0x000C, 0x000D,
          0x0020,
          0x00A0,
          0x1680,
          0x2000, 0x2001, 0x2002, 0x2003, 0x2004, 0x2005, 0x2006, 0x2007, 0x2008, 0x2009, 0x200A,
          0x2028, 0x2029,
          0x202F,
          0x205F,
          0x3000,
          0xFEFF
        );

        (/^\s+$/).test(ws) && (/^\s+$/u).test(ws) && (/^\s+$/v).test(ws)
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn regexp_unicode_mode_rejects_character_class_escape_ranges() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"try { new RegExp("[\\s-a]", "u"); "no"; } catch (e) { e.name }"#)
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "SyntaxError");

  let value = rt
    .exec_script(r#"try { new RegExp("[\\s-a]", "v"); "no"; } catch (e) { e.name }"#)
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "SyntaxError");
}

#[test]
fn regexp_control_escape_cx_matches_control_character() {
  let mut rt = new_runtime();

  assert_eq!(
    rt.exec_script(r#"new RegExp("\\cA").test("\u0001")"#).unwrap(),
    Value::Bool(true)
  );

  assert_eq!(
    rt.exec_script(r#"new RegExp("[\\cA]").test("\u0001")"#).unwrap(),
    Value::Bool(true)
  );

  // Annex B: when `\c` is not followed by an ASCII letter, the `\` is treated as a literal and
  // the following `c` is treated as a normal character (so the pattern matches the two-character
  // string `\c`).
  assert_eq!(
    rt.exec_script(r#"new RegExp("\\c").test("\\c")"#).unwrap(),
    Value::Bool(true)
  );
  assert_eq!(
    rt.exec_script(r#"new RegExp("\\c").test("c")"#).unwrap(),
    Value::Bool(false)
  );

  // Same Annex B behaviour in character classes: `/[\\c]/` matches both `\` and `c`.
  assert_eq!(
    rt.exec_script(r#"new RegExp("[\\c]").test("\\")"#).unwrap(),
    Value::Bool(true)
  );
  assert_eq!(
    rt.exec_script(r#"new RegExp("[\\c]").test("c")"#).unwrap(),
    Value::Bool(true)
  );

  // In non-UnicodeMode character classes, `\c` control escapes also accept decimal digits and `_`
  // (`ClassControlLetter`).
  assert_eq!(
    rt.exec_script(r#"new RegExp("[\\c0]").test("\u0010")"#).unwrap(),
    Value::Bool(true)
  );

  let value = rt
    .exec_script(r#"try { new RegExp("\\c", "u"); "no"; } catch (e) { e.name }"#)
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "SyntaxError");

  let value = rt
    .exec_script(r#"try { new RegExp("[\\c0]", "u"); "no"; } catch (e) { e.name }"#)
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "SyntaxError");
}

#[test]
fn regexp_constructor_pattern_string_allows_line_terminators() {
  let mut rt = new_runtime();

  let value = rt.exec_script(r#"new RegExp("\n").test("\n")"#).unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt
    .exec_script(r#"new RegExp("\u2028").test("\u2028")"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn regexp_dot_does_not_match_line_terminators_without_dotall() {
  let mut rt = new_runtime();

  let value = rt.exec_script(r#"new RegExp(".").test("\n")"#).unwrap();
  assert_eq!(value, Value::Bool(false));

  let value = rt.exec_script(r#"new RegExp(".", "s").test("\n")"#).unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn regexp_prototype_source_escapes_slash_and_line_terminators() {
  let mut rt = new_runtime();

  // `%RegExp.prototype%` special-case.
  let value = rt
    .exec_script(r#"Object.getOwnPropertyDescriptor(RegExp.prototype, "source").get.call(RegExp.prototype)"#)
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "(?:)");

  let value = rt.exec_script(r#"new RegExp("/").source"#).unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "\\/");

  let value = rt.exec_script(r#"new RegExp("\n").source"#).unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "\\n");

  let value = rt.exec_script(r#"new RegExp("\r").source"#).unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "\\r");

  let value = rt.exec_script(r#"new RegExp("\u2028").source"#).unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "\\u2028");

  let value = rt.exec_script(r#"new RegExp("\u2029").source"#).unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "\\u2029");

  // The escaped source should be safe to embed directly in a RegExp literal.
  let value = rt
    .exec_script(r#"eval("/" + new RegExp("/").source + "/").test("/")"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));

  let value = rt
    .exec_script(r#"eval("/" + new RegExp("\n").source + "/").test("\n")"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn regexp_prototype_source_flags_and_bool_getters_special_case_prototype() {
  let mut rt = new_runtime();

  let v = rt
    .exec_script(
      r#"
        Object.getOwnPropertyDescriptor(RegExp.prototype, 'source')
          .get.call(RegExp.prototype) === '(?:)'
      "#,
    )
    .unwrap();
  assert_eq!(v, Value::Bool(true));

  let v = rt
    .exec_script(
      r#"
        Object.getOwnPropertyDescriptor(RegExp.prototype, 'global')
          .get.call(RegExp.prototype) === undefined
      "#,
    )
    .unwrap();
  assert_eq!(v, Value::Bool(true));

  let v = rt
    .exec_script(
      r#"
        Object.getOwnPropertyDescriptor(RegExp.prototype, 'ignoreCase')
          .get.call(RegExp.prototype) === undefined
      "#,
    )
    .unwrap();
  assert_eq!(v, Value::Bool(true));

  let v = rt
    .exec_script(
      r#"
        Object.getOwnPropertyDescriptor(RegExp.prototype, 'hasIndices')
          .get.call(RegExp.prototype) === undefined
      "#,
    )
    .unwrap();
  assert_eq!(v, Value::Bool(true));

  let v = rt
    .exec_script(
      r#"
        Object.getOwnPropertyDescriptor(RegExp.prototype, 'flags')
          .get.call(RegExp.prototype) === ''
      "#,
    )
    .unwrap();
  assert_eq!(v, Value::Bool(true));

  let v = rt
    .exec_script(
      r#"
        Object.getOwnPropertyDescriptor(RegExp.prototype, 'flags')
          .get.call({ hasIndices: true }) === 'd'
      "#,
    )
    .unwrap();
  assert_eq!(v, Value::Bool(true));
}

#[test]
fn regexp_prototype_to_string_is_generic() {
  let mut rt = new_runtime();
  let v = rt
    .exec_script(r#"RegExp.prototype.toString.call({ source: 'a', flags: 'g' }) === '/a/g'"#)
    .unwrap();
  assert_eq!(v, Value::Bool(true));
}

#[test]
fn regexp_lookbehind_with_literal_gt_does_not_parse_as_named_capture_group() {
  let mut rt = new_runtime();

  // `(?<=>)` is a lookbehind whose body is the literal `>`. It must *not* be parsed as a named
  // capturing group with name `=` terminated by the `>` from the body.
  let v = rt.exec_script(r#"/(?<=>)a/.test("a")"#).unwrap();
  assert_eq!(v, Value::Bool(false));
  let v = rt.exec_script(r#"/(?<=>)a/.test(">a")"#).unwrap();
  assert_eq!(v, Value::Bool(true));

  // Same pitfall for negative lookbehind: `(?<!a>)` must not treat `!a` as a group name.
  let v = rt.exec_script(r#"/(?<!a>)a$/.test("a")"#).unwrap();
  assert_eq!(v, Value::Bool(true));
  let v = rt.exec_script(r#"/(?<!a>)a$/.test("a>a")"#).unwrap();
  assert_eq!(v, Value::Bool(false));
}

#[test]
fn regexp_lookbehind_variable_length() {
  // From test262 `lookBehind/variable-length.js`.
  let mut rt = new_runtime();
  let v = rt
    .exec_script(
      r#"
        "abcdef".match(/(?<=[a|b|c]*)[^a|b|c]{3}/)[0] === "def" &&
        "abcdef".match(/(?<=\w*)[^a|b|c]{3}/)[0] === "def"
      "#,
    )
    .unwrap();
  assert_eq!(v, Value::Bool(true));
}

#[test]
fn regexp_lookbehind_global_exec_merges_captures() {
  // From test262 `lookBehind/sticky.js`.
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var re = /(?<=^(\w+))def/g;
        var a = re.exec("abcdefdef");
        var b = re.exec("abcdefdef");
        [a[0], a[1], b[0], b[1], re.lastIndex].join(",")
      "#,
    )
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "def,abc,def,abcdef,9");
}

#[test]
fn regexp_lookbehind_alternations_ordering_and_atomicity() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        function m(s, r) {
          var res = s.match(r);
          return res === null ? "null" : JSON.stringify(res);
        }
        [
          m("xabcd", /.*(?<=(..|...|....))(.*)/),
          m("xabcd", /.*(?<=(xx|...|....))(.*)/),
          m("xxabcd", /.*(?<=(xx|...))(.*)/),
          m("xxabcd", /.*(?<=(xx|xxx))(.*)/),
        ].join("|")
      "#,
    )
    .unwrap();

  assert_eq!(
    as_utf8_lossy(&rt, value),
    r#"["xabcd","cd",""]|["xabcd","bcd",""]|["xxabcd","bcd",""]|["xxabcd","xx","abcd"]"#
  );
}

#[test]
fn regexp_lookbehind_direction_minus_one_backref_before_capture_allows_greedy_growth() {
  // From test262: back-references-to-captures.js#6
  //
  // This relies on spec-accurate right-to-left (direction=-1) evaluation inside the lookbehind:
  // the backreference is evaluated before the capture group when matching backwards, so it starts
  // as empty and allows the capture group to grow greedily to its maximal value.
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"JSON.stringify("ababc".match(/(?<=(\w+)\1)c/))"#)
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), r#"["c","abab"]"#);
}

#[test]
fn regexp_lookbehind_direction_minus_one_forward_reference_backref_respects_ignore_case() {
  // From test262: back-references-to-captures.js#1
  //
  // A forward reference (`\1`) inside a lookbehind: the capture runs first (right-to-left), and
  // the backreference should be evaluated with ignoreCase semantics.
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"JSON.stringify("abcCd".match(/(?<=\1(\w))d/i))"#)
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), r#"["d","C"]"#);
}

#[test]
fn regexp_lookbehind_direction_minus_one_forward_reference_backref_sees_capture() {
  // From test262: back-references-to-captures.js#2
  //
  // `\1` is a forward reference to a capture that appears later in the lookbehind pattern.
  // With direction=-1 evaluation, the capture runs first (right-to-left), so the backreference
  // sees the captured value.
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"JSON.stringify("abxxd".match(/(?<=\1([abx]))d/))"#)
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), r#"["d","x"]"#);
}

#[test]
fn regexp_lookbehind_direction_minus_one_forward_reference_backref_greedy_capture_backtracks() {
  // From test262: back-references-to-captures.js#3-#5
  //
  // This is a forward reference (`\1`) to a greedy capture group. With direction=-1 matching, the
  // capture is evaluated first and must backtrack so that `\1` can match the same substring.
  //
  // A naive implementation that runs the lookbehind body forward from a start index can treat `\1`
  // as empty and incorrectly succeed with an over-large capture, or succeed when it should fail.
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        [
          JSON.stringify("ababc".match(/(?<=\1(\w+))c/)),
          JSON.stringify("ababbc".match(/(?<=\1(\w+))c/)),
          JSON.stringify("ababdc".match(/(?<=\1(\w+))c/)),
        ].join("|")
      "#,
    )
    .unwrap();
  assert_eq!(
    as_utf8_lossy(&rt, value),
    r#"["c","ab"]|["c","b"]|null"#
  );
}

#[test]
fn regexp_lookbehind_positive_basic() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"'abcdef'.match(/(?<=abc)\w\w\w/)[0] === 'def'"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn regexp_lookbehind_negative_basic() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        ('abcdef'.match(/(?<!abc)def/) === null) &&
        ('abcdef'.match(/(?<!abc)\w\w\w/)[0] === 'abc')
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn regexp_lookbehind_captures_from_positive_propagate() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"'abcdef'.match(/(?<=(c))def/)[1] === 'c'"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn regexp_lookbehind_captures_from_negative_do_not_propagate() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var m = 'abcdef'.match(/(?<!(^|[ab]))\w{2}/);
        m.length === 2 && m[1] === undefined
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn regexp_lookbehind_is_atomic_no_backtracking_into_assertion() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"'abcdbc'.match(/(?<=([abc]+)).\1/) === null"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn regexp_lookbehind_nested_lookaround_sanity() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"'abcdef'.match(/(?<=ab(?=c)\wd)\w\w/)[0] === 'ef'"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn regexp_lookbehind_sliced_strings_do_not_read_before_start() {
  // Adapted from test262 `built-ins/RegExp/lookBehind/sliced-strings.js`.
  //
  // When a string is produced via `slice` / `substring`, engines often share the backing storage.
  // Lookbehind must treat the sliced string's logical start as index 0 and never read earlier
  // code units, even if they are adjacent in memory.
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var oob_subject = "abcdefghijklmnabcdefghijklmn".slice(14);
        [
          oob_subject.match(/(?=(abcdefghijklmn))(?<=\1)a/i) === null,
          oob_subject.match(/(?=(abcdefghijklmn))(?<=\1)a/) === null,
          "abcdefgabcdefg".slice(1).match(/(?=(abcdefg))(?<=\1)/) === null,
        ].join(",")
      "#,
    )
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "true,true,true");
}

#[test]
fn regexp_unicode_mode_rejects_legacy_octal_and_oob_backrefs() {
  let mut rt = new_runtime();

  let value = rt
    .exec_script(
      r#"
        function errName(thunk) {
          try { thunk(); return "no"; } catch (e) { return e.name; }
        }
        [
          errName(() => new RegExp("\\1", "u")),
          errName(() => new RegExp("\\1", "v")),
          errName(() => new RegExp("\\8", "u")),
          errName(() => new RegExp("\\8", "v")),
          errName(() => new RegExp("(a)\\10", "u")),
          errName(() => new RegExp("(a)\\10", "v")),
          errName(() => new RegExp("\\00", "u")),
          errName(() => new RegExp("\\00", "v")),
          errName(() => new RegExp("[\\00]", "u")),
          errName(() => new RegExp("[\\00]", "v")),
          errName(() => eval('/\\1/u')),
          errName(() => eval('/\\1/v')),
        ].join(",")
      "#,
    )
    .unwrap();

  assert_eq!(
    as_utf8_lossy(&rt, value),
    "SyntaxError,SyntaxError,SyntaxError,SyntaxError,SyntaxError,SyntaxError,SyntaxError,SyntaxError,SyntaxError,SyntaxError,SyntaxError,SyntaxError"
  );
}

#[test]
fn regexp_lookbehind_misc_anchor_interactions() {
  let mut rt = new_runtime();

  // Ported from test262 `built-ins/RegExp/lookBehind/misc.js`.
  let value = rt
    .exec_script(
      r#"
        [
          "abcdef".match(/(?<=$abc)def/) === null,
          "fno".match(/^f.o(?<=foo)$/) === null,
          "foo".match(/^foo(?<!foo)$/) === null,
          "foo".match(/^f.o(?<!foo)$/) === null,
          "foo".match(/^foo(?<=foo)$/)[0] === "foo",
          "foo".match(/^f.o(?<=foo)$/)[0] === "foo",
          "fno".match(/^f.o(?<!foo)$/)[0] === "fno",
          "foooo".match(/^foooo(?<=fo+)$/)[0] === "foooo",
          "foooo".match(/^foooo(?<=fo*)$/)[0] === "foooo",
        ].every((x) => x === true)
      "#,
    )
    .unwrap();

  assert_eq!(value, Value::Bool(true));
}

#[test]
fn regexp_lookbehind_can_reference_prior_captures() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"JSON.stringify("abb".match(/(.)(?<=(\1\1))/))"#)
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), r#"["b","b","bb"]"#);
}

#[test]
fn regexp_captures_from_lookbehind_visible_to_later_backrefs() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"JSON.stringify("  'foo'  ".match(/(?<=(.))(\w+)(?=\1)/))"#)
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), r#"["foo","'","foo"]"#);

  let value = rt
    .exec_script(r#"JSON.stringify('  "foo"  '.match(/(?<=(.))(\w+)(?=\1)/))"#)
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), r#"["foo","\"","foo"]"#);
}

#[test]
fn regexp_lookbehind_greedy_quantifiers_capture_maximal_left_context() {
  // From test262 `lookBehind/greedy-loop.js`.
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        [
          JSON.stringify("abbbbbbc".match(/(?<=(b+))c/)),
          JSON.stringify("ab1234c".match(/(?<=(b\d+))c/)),
          JSON.stringify("ab12b23b34c".match(/(?<=((?:b\d{2})+))c/)),
        ].join("|")
      "#,
    )
    .unwrap();

  assert_eq!(
    as_utf8_lossy(&rt, value),
    r#"["c","bbbbbb"]|["c","b1234"]|["c","b12b23b34"]"#
  );
}

#[test]
fn regexp_lookbehind_mutual_recursive_backreferences_use_empty_for_unset_captures() {
  // From test262 `lookBehind/mutual-recursive.js`.
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        [
          JSON.stringify(/(?<=a(.\2)b(\1)).{4}/.exec("aabcacbc")),
          JSON.stringify(/(?<=a(\2)b(..\1))b/.exec("aacbacb")),
          JSON.stringify(/(?<=(?:\1b)(aa))./.exec("aabaax")),
          JSON.stringify(/(?<=(?:\1|b)(aa))./.exec("aaaax")),
        ].join("|")
      "#,
    )
    .unwrap();

  assert_eq!(
    as_utf8_lossy(&rt, value),
    r#"["cacb","a",""]|["b","ac","ac"]|["x","aa"]|["x","aa"]"#
  );
}

#[test]
fn regexp_lookbehind_ignore_case_backref_inside_lookbehind() {
  // Adapted from test262 `lookBehind/back-references.js`.
  //
  // Regression test: Ensure ignoreCase comparisons in lookbehind use the same canonicalization as
  // forward matching, including for backreferences and for captures created inside the lookbehind.
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"JSON.stringify("abB".match(/(.)(?<=(\1\1))/i))"#)
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), r#"["B","B","bB"]"#);
}

#[test]
fn regexp_lookbehind_ignore_case_cross_lookaround_captures() {
  // Adapted from test262 `lookBehind/back-references.js`.
  //
  // Regression test: A capture from a lookahead must be visible to a subsequent lookbehind, and
  // the backreference match performed inside the lookbehind must respect ignoreCase comparisons.
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"JSON.stringify("abaBbAa".match(/(?=(\w))(?<=(\1))./i))"#)
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), r#"["b","b","B"]"#);
}

#[test]
fn regexp_lookbehind_word_boundary_assertions() {
  // From test262 `lookBehind/word-boundary.js`.
  let mut rt = new_runtime();

  let value = rt
    .exec_script(r#""abc def".match(/(?<=\b)[d-f]{3}/)[0]"#)
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "def");

  let value = rt
    .exec_script(r#""ab cdef".match(/(?<=\B)\w{3}/)[0]"#)
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "def");

  let value = rt.exec_script(r#""abcdef".match(/(?<=\b)[d-f]{3}/)"#).unwrap();
  assert_eq!(value, Value::Null);
}

#[test]
fn regexp_lookbehind_start_and_end_of_line_assertions_multiline_global() {
  // From test262 `lookBehind/start-of-line.js`.
  let mut rt = new_runtime();

  let value = rt
    .exec_script(r#""xyz\nabcdef".match(/(?<=^[a-c]{3})def/m)[0]"#)
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "def");

  let value = rt
    .exec_script(r#""ab\ncd\nefg".match(/(?<=^)\w+/gm).join(",")"#)
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "ab,cd,efg");

  // End-of-line lookbehind already direction-independent, but this exercises `$` integration inside
  // lookbehind with `m`/`g`.
  let value = rt
    .exec_script(r#""ab\ncd\nefg".match(/\w+(?<=$)/gm).join(",")"#)
    .unwrap();
  assert_eq!(as_utf8_lossy(&rt, value), "ab,cd,efg");
}
