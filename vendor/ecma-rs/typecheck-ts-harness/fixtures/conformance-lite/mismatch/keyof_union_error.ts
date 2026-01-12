// @lib: es5

type A = { a: number };
type B = { b: string };

type K = keyof (A | B);

const k: K = "a";
