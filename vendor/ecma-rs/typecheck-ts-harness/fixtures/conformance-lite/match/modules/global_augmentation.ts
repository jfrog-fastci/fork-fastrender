// @lib: es5
// @filename: augment.ts
export {};

declare global {
  interface GlobalFoo {
    a: number;
  }
}

// @filename: main.ts
const x: GlobalFoo = { a: 1 };
const n: number = x.a;
void n;
