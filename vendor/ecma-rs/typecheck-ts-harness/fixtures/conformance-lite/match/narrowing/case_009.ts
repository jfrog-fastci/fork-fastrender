// @lib: es5

type T = { tag: "a"; a: number } | { tag: "b"; b: string };

function f(v: T) {
  if (v.tag === "a") {
    const n: number = v.a;
    return n + 9;
  }
  const s: string = v.b;
  return s;
}

f({ tag: "a", a: 9 });
f({ tag: "b", b: "ok" });
