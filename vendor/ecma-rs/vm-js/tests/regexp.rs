use vm_js::{Budget, Heap, HeapLimits, JsRuntime, TerminationReason, Value, Vm, VmError, VmOptions};

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

