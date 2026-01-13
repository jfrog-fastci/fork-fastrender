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
fn regexp_literal_with_uv_flags_is_early_error() {
  let mut rt = new_runtime();
  let err = rt.exec_script(r#"var r = /./uv;"#).unwrap_err();
  assert!(matches!(err, VmError::Syntax(_)));
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
          return res === null ? "null" : res.join(",");
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
    "xabcd,cd,|xabcd,bcd,|xxabcd,bcd,|xxabcd,xx,abcd"
  );
}
