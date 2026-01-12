// @lib: es5

type Obj = { a: number; b: string };

type ReadonlyObj = { readonly [K in keyof Obj]: Obj[K] };

declare const o: ReadonlyObj;

const a: number = o.a;
const b: string = o.b;
void a;
void b;
