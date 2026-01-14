// Generators + yield/yield* in non-trivial control flow.
function* g() {
  let x = yield 1;
  try {
    yield* [x, x + 1];
  } catch (e) {
    yield "catch";
  } finally {
    yield "finally";
  }
  return 0;
}

const it = g();
it.next();
it.next(10);
it.throw("boom");
it.return(0);

