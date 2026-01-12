// @lib: es5

type HasIndex = { [k: string]: number };
type HasKnown = { a: number; b: number };

const v: HasKnown = { a: 4, b: 5 };
const idx: HasIndex = v;

const n: number = idx["a"];
void n;
