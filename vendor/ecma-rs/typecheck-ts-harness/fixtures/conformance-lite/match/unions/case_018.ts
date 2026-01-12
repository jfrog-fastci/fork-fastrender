// @lib: es5

type U = string | number;

const a: U = "x";
const b: U = 18;

// Preserve union in inferred types.
const v = b ? a : b;
const w: U = v;
void w;
