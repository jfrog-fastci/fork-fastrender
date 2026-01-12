// @lib: es5

type A = { a: number; common: string };
type B = { b: number; common: string };

type U = A | B;

function get(v: U) {
  const c: string = v.common;
  if ("a" in v) {
    const n: number = v.a;
    return n + 13;
  }
  const m: number = v.b;
  return m + 13;
}

get({ a: 13, common: "x" });
get({ b: 14, common: "y" });
