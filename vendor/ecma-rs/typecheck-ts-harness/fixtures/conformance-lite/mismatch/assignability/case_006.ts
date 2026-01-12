// @lib: es5

type Wide = (x: string | number) => void;
type Narrow = (x: string) => void;

const narrow: Narrow = (x) => { void x; };

// With strictFunctionTypes, parameter positions are checked contravariantly.
const wide: Wide = narrow;
void wide;
