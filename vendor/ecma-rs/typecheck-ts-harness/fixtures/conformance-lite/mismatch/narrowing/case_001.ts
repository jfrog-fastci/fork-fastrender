// @lib: es5

function f(v: string | number) {
  const s: string = v;
  void s;
}

f(1);
