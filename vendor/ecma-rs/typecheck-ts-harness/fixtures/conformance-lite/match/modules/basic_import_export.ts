// @lib: es5
// @filename: a.ts
export interface Foo {
  a: number;
  b: string;
}
export const value = 1;

// @filename: b.ts
import { Foo, value } from "./a";

const x: Foo = { a: value, b: "ok" };
const n: number = x.a;
const s: string = x.b;
void n;
void s;
