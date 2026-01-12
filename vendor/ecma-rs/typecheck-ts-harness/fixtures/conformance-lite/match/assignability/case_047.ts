// @lib: es5

type A = { a: number };
type B = { b: string };
type C = A & B;

const c: C = { a: 47, b: "ok" };
const a: A = c;
const b: B = c;

const n: number = a.a;
const s: string = b.b;
void n;
void s;
