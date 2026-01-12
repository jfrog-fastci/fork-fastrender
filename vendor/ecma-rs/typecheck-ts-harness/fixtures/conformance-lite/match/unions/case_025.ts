// @lib: es5

type A = { a: number; common: string };
type B = { b: number; common: string };

type U = A | B;

function get(v: U) {
  const c: string = v.common;
  if ("a" in v) {
    const n: number = v.a;
    return n + 25;
  }
  const m: number = v.b;
  return m + 25;
}

get({ a: 25, common: "x" });
get({ b: 26, common: "y" });
