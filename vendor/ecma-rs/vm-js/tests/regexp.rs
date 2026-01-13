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

  // Nested quantifier: the inner optional `(b)?` capture must also be cleared between `{2}`
  // iterations.
  let value = rt
    .exec_script(r#""cba".match(/(?:(a)|c(b)?){2}/)[2]"#)
    .unwrap();
  assert_eq!(value, Value::Undefined);
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
