use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
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
fn array_spread() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(r#"var a=[1,2]; var b=[0,...a,3]; b.length === 4 && b[1]===1 && b[3]===3"#)
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
fn array_spread_closes_iterator_on_throw() {
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
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn call_spread_closes_iterator_on_throw() {
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
  assert_eq!(value, Value::Bool(true));
}
