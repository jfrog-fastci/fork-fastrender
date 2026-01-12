// @lib: es5

type Dist<T> = T extends string ? "s" : "n";
type R = Dist<string | number>;

export const a: R = "s";
export const b: R = "n";

type NonDist<T> = [T] extends [string] ? "s" : "n";
type R2 = NonDist<string | number>;

export const c: R2 = "n";
