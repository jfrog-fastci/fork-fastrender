// @lib: es5

type A = { a: number };
type B = { a: number; b: string };

const b: B = { a: 9, b: "ok" };
const a: A = b;

const n: number = a.a;
void n;
