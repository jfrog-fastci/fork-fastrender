// @lib: es5
// @filename: pkg.ts
export interface Foo {
  a: number;
}

// @filename: augment.ts
export {};

declare module "./pkg" {
  interface Foo {
    b: string;
  }
}

// @filename: main.ts
import { Foo } from "./pkg";

const v: Foo = { a: 1, b: "ok" };
const s: string = v.b;
void s;
