use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  // Iterator/generator features (and their associated intrinsics) have a non-trivial baseline
  // memory footprint. Use a small 2MiB heap budget so these tests don't fail with OOM while still
  // keeping memory limits tight.
  let heap = Heap::new(HeapLimits::new(2 * 1024 * 1024, 2 * 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

fn assert_value_is_utf8(rt: &JsRuntime, value: Value, expected: &str) {
  let Value::String(s) = value else {
    panic!("expected string, got {value:?}");
  };
  let actual = rt.heap().get_string(s).unwrap().to_utf8_lossy();
  assert_eq!(actual, expected);
}

#[test]
fn for_of_over_array() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var s=0; for (var x of [1,2,3]) { s = s + x; } s"#)
    .unwrap();
  assert_eq!(value, Value::Number(6.0));
}

#[test]
fn for_of_break_calls_iterator_return_on_custom_array_iterator() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var returnCalls = 0;
      const arr = [1, 2, 3];
      arr[Symbol.iterator] = function () {
        let i = 0;
        return {
          next() {
            if (i >= 3) return { value: undefined, done: true };
            return { value: i++, done: false };
          },
          return() {
            returnCalls++;
            return { done: true };
          },
        };
      };

      for (const x of arr) {
        break;
      }
      returnCalls === 1
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn for_of_throw_calls_iterator_return_on_custom_array_iterator() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var returnCalls = 0;
      var out = "";
      const arr = [1, 2, 3];
      arr[Symbol.iterator] = function () {
        let i = 0;
        return {
          next() {
            if (i >= 3) return { value: undefined, done: true };
            return { value: i++, done: false };
          },
          return() {
            returnCalls++;
            return { done: true };
          },
        };
      };

      try {
        for (const x of arr) {
          throw "boom";
        }
      } catch (e) {
        out = e;
      }

      out === "boom" && returnCalls === 1
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn for_of_break_does_not_call_array_return_getter_with_default_iterator() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      const arr = [1, 2, 3];
      Object.defineProperty(arr, "return", {
        get() { throw "wrong"; },
      });

      var out = "ok";
      try {
        for (const x of arr) {
          break;
        }
      } catch (e) {
        out = e;
      }

      out === "ok"
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn for_of_over_string() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var s=""; for (var c of "ab") { s = s + c; } s"#)
    .unwrap();
  assert_value_is_utf8(&rt, value, "ab");
}

#[test]
fn for_of_over_array_grows_during_iteration() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var a=[1,2]; var out=[];
      for (var x of a) { out.push(x); if (x===1) a.push(3); }
      out.join(',')
    "#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "1,2,3");
}

#[test]
fn for_of_over_array_shrinks_during_iteration() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var a=[1,2,3]; var out=[];
      for (var x of a) { out.push(x); a.length = 1; }
      out.join(',')
    "#,
    )
    .unwrap();
  assert_value_is_utf8(&rt, value, "1");
}

#[test]
fn for_of_does_not_close_iterator_when_next_throws() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var closed = 0;
      var iterable = {};
      iterable[Symbol.iterator] = function () {
        return {
          next: function () { throw 1; },
          "return": function () { closed = closed + 1; return { done: true }; },
        };
      };
      try { for (var x of iterable) {} } catch (e) {}
      closed
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Number(0.0));
}

#[test]
fn for_of_does_not_close_iterator_when_next_throws_in_async_eval() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var closed = 0;
      var iterable = {};
      iterable[Symbol.iterator] = function () {
        return {
          next: function () { throw 1; },
          "return": function () { closed = closed + 1; return { done: true }; },
        };
      };

      async function f() {
        try {
          for (var x of iterable) { await 0; }
        } catch (e) {}
      }

      f();
      closed
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Number(0.0));
}

#[test]
fn array_spread() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var a=[1,2]; var b=[0,...a,3]; b.length === 4 && b[1]===1 && b[3]===3"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_spread_invokes_prototype_accessors_for_holes() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var called = 0;
        Object.defineProperty(Array.prototype, "1", { get: function(){ called++; return 99; }, configurable: true });
        var a = [1];
        a.length = 2;
        var out = [...a];
        called === 1 && out.length === 2 && out[0] === 1 && out[1] === 99
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn string_spread_iterates_code_points() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"var a=[..."a\uD834\uDF06b"]; a.length===3 && a[0]==="a" && a[1]==="\uD834\uDF06" && a[2]==="b""#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn call_spread() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"function add(a,b,c){ return a+b+c; } add(...[1,2,3])"#)
    .unwrap();
  assert_eq!(value, Value::Number(6.0));
}

#[test]
fn call_spread_over_string() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"function join(a,b){ return a+b; } join(...("ab"))"#)
    .unwrap();
  assert_value_is_utf8(&rt, value, "ab");
}

#[test]
fn array_spread_does_not_close_iterator_on_throw() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var closed = false;
        var iterable = {
          [Symbol.iterator]: function () {
            return {
              i: 0,
              next: function () {
                if (this.i++ === 0) return { value: 1, done: false };
                throw "boom";
              },
              return: function () {
                closed = true;
                return { done: true };
              },
            };
          },
        };

        try { [...iterable]; } catch (e) {}
        closed
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(false));
}

#[test]
fn call_spread_does_not_close_iterator_on_throw() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var closed = false;
        var iterable = {
          [Symbol.iterator]: function () {
            return {
              i: 0,
              next: function () {
                if (this.i++ === 0) return { value: 1, done: false };
                throw "boom";
              },
              return: function () {
                closed = true;
                return { done: true };
              },
            };
          },
        };

        function f() {}
        try { f(...iterable); } catch (e) {}
        closed
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(false));
}

#[test]
fn array_spread_does_not_close_iterator_on_throw_immediately() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
        var closed = false;
        var iterable = {
          [Symbol.iterator]: function () {
            return {
              next: function () { throw "boom"; },
              return: function () {
                closed = true;
                return { done: true };
              },
            };
          },
        };

        try { [...iterable]; } catch (e) {}
        closed
      "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(false));
}

#[test]
fn for_of_over_array_iterator() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var s=0; for (var x of [1,2].values()) { s = s + x; } s"#)
    .unwrap();
  assert_eq!(value, Value::Number(3.0));
}

#[test]
fn array_iterator_is_iterable() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var it = [1,2].values(); it[Symbol.iterator]() === it"#)
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn for_of_over_string_iterator() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var s=""; for (var c of "ab"[Symbol.iterator]()) { s = s + c; } s"#)
    .unwrap();
  assert_value_is_utf8(&rt, value, "ab");
}

#[test]
fn spread_over_array_iterator() {
  let mut rt = new_runtime();
  let value = rt.exec_script(r#"[...[1,2].values()].length === 2"#).unwrap();
  assert_eq!(value, Value::Bool(true));
}
