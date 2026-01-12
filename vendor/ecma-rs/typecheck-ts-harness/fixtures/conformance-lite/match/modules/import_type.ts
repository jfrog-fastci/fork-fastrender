// @lib: es5
// @filename: a.ts
export interface Foo {
  a: number;
}

// @filename: b.ts
import type { Foo } from "./a";
const x: Foo = { a: 1 };
const n: number = x.a;
void n;
