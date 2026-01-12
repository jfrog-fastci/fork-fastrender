// @lib: es5

type A = { readonly a: number };
type B = { a: number };

const b: B = { a: 11 };
const a: A = b;

const v: number = a.a;
void v;
