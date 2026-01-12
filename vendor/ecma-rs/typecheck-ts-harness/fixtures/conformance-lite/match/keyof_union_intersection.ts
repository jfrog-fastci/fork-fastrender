// @lib: es5

type A = { a: number };
type B = { b: string };

type KeysUnion = keyof (A | B);
type KeysIntersection = keyof (A & B);

type IsNever<T> = [T] extends [never] ? true : false;

// `keyof` over unions yields common keys only.
export const union_keys_is_never: IsNever<KeysUnion> = true;

export const a_key: KeysIntersection = "a";
export const b_key: KeysIntersection = "b";
