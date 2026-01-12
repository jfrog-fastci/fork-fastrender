// @lib: es5

type A = { a: number };
type B = { b: string };

type AB = A & B;

export const merged: AB = { a: 1, b: "x" };
