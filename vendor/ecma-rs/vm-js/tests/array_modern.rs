use vm_js::{Heap, HeapLimits, JsRuntime, Value, Vm, VmError, VmOptions};

fn new_runtime() -> JsRuntime {
  let vm = Vm::new(VmOptions::default());
  let heap = Heap::new(HeapLimits::new(1024 * 1024, 1024 * 1024));
  JsRuntime::new(vm, heap).unwrap()
}

#[test]
fn array_modern_methods_work() -> Result<(), VmError> {
  let mut rt = new_runtime();
  let value = rt.exec_script(
    r#"
    var ok = true;

    // --- Array.prototype.at ---
    var a = [1, 2, 3];
    ok = ok && a.at(0) === 1;
    ok = ok && a.at(-1) === 3;
    ok = ok && a.at(3) === undefined;
    ok = ok && Array.prototype.at.call({ length: 2, 0: "a", 1: "b" }, 1) === "b";

    // --- Array.prototype.flat ---
    var f = [1, [2, [3]], , 4];
    var f1 = f.flat();
    ok = ok && f1.length === 4 && f1[0] === 1 && f1[1] === 2 && Array.isArray(f1[2]) && f1[2][0] === 3 && f1[3] === 4;
    var f2 = f.flat(2);
    ok = ok && f2.length === 4 && f2[0] === 1 && f2[1] === 2 && f2[2] === 3 && f2[3] === 4;
    var f0 = f.flat(0);
    ok = ok && f0.length === 3 && f0[0] === 1 && Array.isArray(f0[1]) && f0[2] === 4;

    // ArraySpeciesCreate (flat): respect @@species and pass length=0.
    var called = false;
    var seenLen = -1;
    function SpeciesCtor(len) {
      called = true;
      seenLen = len;
      return [];
    }
    var s = [1, [2]];
    s.constructor = {};
    s.constructor[Symbol.species] = SpeciesCtor;
    var sFlat = s.flat();
    ok = ok && called && seenLen === 0;
    ok = ok && sFlat.length === 2 && sFlat[0] === 1 && sFlat[1] === 2;

    // --- Array.prototype.flatMap ---
    var count = 0;
    var fm = [1, , 2].flatMap(function(x) {
      count++;
      return [x, x * 2];
    });
    ok = ok && count === 2;
    ok = ok && fm.length === 4 && fm[0] === 1 && fm[1] === 2 && fm[2] === 2 && fm[3] === 4;

    // ArraySpeciesCreate (flatMap): respect @@species and pass length=0, plus thisArg.
    called = false;
    seenLen = -1;
    function SpeciesCtor2(len) {
      called = true;
      seenLen = len;
      return [];
    }
    var s2 = [1, 2];
    s2.constructor = {};
    s2.constructor[Symbol.species] = SpeciesCtor2;
    var thisArg = { mult: 10 };
    var fm2 = s2.flatMap(function(x) { return [x * this.mult]; }, thisArg);
    ok = ok && called && seenLen === 0;
    ok = ok && fm2.length === 2 && fm2[0] === 10 && fm2[1] === 20;

    // --- Array.prototype.findLast / findLastIndex ---
    var fl = [1, , 2, 3, 2];
    ok = ok && fl.findLast(function(x) { return x === 2; }) === 2;
    ok = ok && fl.findLastIndex(function(x) { return x === 2; }) === 4;
    ok = ok && fl.findLastIndex(function(x) { return x === 99; }) === -1;

    // --- Array.prototype.toReversed ---
    var tr = [0, , 2, , 4];
    Array.prototype[3] = 3;
    var tr2 = tr.toReversed();
    ok = ok && tr2.length === 5 && tr2[0] === 4 && tr2[1] === 3 && tr2[2] === 2 && tr2[3] === undefined && tr2[4] === 0;
    ok = ok && tr2.hasOwnProperty("3");
    delete Array.prototype[3];

    // toReversed ignores @@species.
    called = false;
    tr.constructor = {};
    tr.constructor[Symbol.species] = function() { called = true; return []; };
    var tr3 = tr.toReversed();
    ok = ok && called === false;
    ok = ok && Object.getPrototypeOf(tr3) === Array.prototype;

    // --- Array.prototype.toSorted ---
    var ts = [3, , 4, , 1];
    Array.prototype[3] = 2;
    var sorted = ts.toSorted();
    ok = ok && sorted.length === 5;
    ok = ok && sorted[0] === 1 && sorted[1] === 2 && sorted[2] === 3 && sorted[3] === 4 && sorted[4] === undefined;
    ok = ok && sorted.hasOwnProperty("4");
    ok = ok && !ts.hasOwnProperty("1");
    delete Array.prototype[3];

    // --- Array.prototype.toSpliced ---
    var sp = [1, , 3];
    var sp2 = sp.toSpliced(1, 0, 2);
    ok = ok && sp2.length === 4 && sp2[0] === 1 && sp2[1] === 2 && sp2[2] === undefined && sp2[3] === 3;
    ok = ok && sp2.hasOwnProperty("2");

    // toSpliced with no arguments returns a copy (not an empty array).
    var sp0 = ["a", "b"];
    var sp0Copy = sp0.toSpliced();
    ok = ok && sp0Copy.length === 2 && sp0Copy[0] === "a" && sp0Copy[1] === "b";
    ok = ok && sp0Copy !== sp0;

    // toSpliced works on frozen objects (array + array-like).
    var frozenArr = Object.freeze(["a", "b"]);
    var frozenCopy = frozenArr.toSpliced();
    ok = ok && frozenCopy.length === 2 && frozenCopy[0] === "a" && frozenCopy[1] === "b";
    ok = ok && frozenCopy !== frozenArr;

    var frozenArrayLike = Object.freeze({ length: 2, 0: "a", 1: "b" });
    var frozenArrayLikeCopy = Array.prototype.toSpliced.call(frozenArrayLike);
    ok = ok && Array.isArray(frozenArrayLikeCopy);
    ok = ok && frozenArrayLikeCopy.length === 2 && frozenArrayLikeCopy[0] === "a" && frozenArrayLikeCopy[1] === "b";

    // toSpliced ignores @@species.
    called = false;
    sp.constructor = {};
    sp.constructor[Symbol.species] = function() { called = true; return []; };
    var sp3 = sp.toSpliced(0, 0);
    ok = ok && called === false;

    // --- Array.prototype.with ---
    var w = [0, , 2, , 4];
    Array.prototype[3] = 3;
    var w2 = w.with(2, 6);
    ok = ok && w2.length === 5 && w2[0] === 0 && w2[1] === undefined && w2[2] === 6 && w2[3] === 3 && w2[4] === 4;
    ok = ok && w2.hasOwnProperty("1") && w2.hasOwnProperty("3");
    delete Array.prototype[3];

    var threw = false;
    try { [1, 2].with(2, 0); } catch(e) { threw = e.name === "RangeError"; }
    ok = ok && threw;

    ok
  "#,
  )?;
  assert_eq!(value, Value::Bool(true));
  Ok(())
}
