// @lib: es5

type A = { a?: number };
type B = { a: number };

const b: B = { a: 42 };
const a: A = b;

const v: number | undefined = a.a;
void v;
