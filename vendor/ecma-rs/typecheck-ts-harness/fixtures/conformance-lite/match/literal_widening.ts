// @lib: es5

type IsExactlyString<T> = [T] extends [string]
  ? [string] extends [T]
    ? true
    : false
  : false;

const c = "a";
let l = "a";

// `const` initializers preserve literal types, but `let` initializers widen.
export const const_is_string: IsExactlyString<typeof c> = false;
export const let_is_string: IsExactlyString<typeof l> = true;
