use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn array_constructor_length_validation_throws_range_error() {
  let mut rt = new_runtime();
  let value = rt
    .exec_script(
      r#"
      var ok = true;
      try { Array(3.5); ok = false; } catch(e) { ok = ok && e.name === "RangeError"; }
      try { new Array(3.5); ok = false; } catch(e) { ok = ok && e.name === "RangeError"; }
      try { Array(-1); ok = false; } catch(e) { ok = ok && e.name === "RangeError"; }
      try { Array(4294967296); ok = false; } catch(e) { ok = ok && e.name === "RangeError"; }

      var z = Array(-0);
      ok = ok && z.length === 0;

      var a = Array(2);
      ok = ok && a.length === 2 && !a.hasOwnProperty("0");
      ok
    "#,
    )
    .unwrap();
  assert_eq!(value, Value::Bool(true));
}

#[test]
fn array_prototype_sort_is_stable_and_handles_holes_and_undefined() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    var ok = true;

    // Stable sort (ES2019+): preserve order of elements that compare equal.
    var a = [{k:1,id:"a"},{k:2,id:"b"},{k:1,id:"c"},{k:2,id:"d"}];
    a.sort(function(x, y) { return x.k - y.k; });
    ok = ok && a[0].id === "a" && a[1].id === "c" && a[2].id === "b" && a[3].id === "d";

    // Holes are preserved (deleted) and sort after explicit `undefined`.
    var b = [, undefined, 1];
    b.sort();
    ok = ok && b.length === 3 && b[0] === 1 && b[1] === undefined && !b.hasOwnProperty("2");

    // Default sort is string-based.
    var c = [10, 2];
    var r = c.sort();
    ok = ok && r === c && c[0] === 10 && c[1] === 2;

    // Non-callable compareFn throws TypeError.
    var threw = false;
    try { [].sort(1); } catch(e) { threw = e.name === "TypeError"; }
    ok = ok && threw;

    ok
  "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}

