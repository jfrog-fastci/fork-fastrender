// @lib: es5

type A = { a: number; common: string };
type B = { b: number; common: string };

type U = A | B;

function get(v: U) {
  const c: string = v.common;
  if ("a" in v) {
    const n: number = v.a;
    return n + 5;
  }
  const m: number = v.b;
  return m + 5;
}

get({ a: 5, common: "x" });
get({ b: 6, common: "y" });
