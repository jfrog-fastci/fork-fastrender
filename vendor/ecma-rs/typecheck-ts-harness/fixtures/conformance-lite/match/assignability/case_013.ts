// @lib: es5

type Narrow = (x: string) => void;
type Wide = (x: string | number) => void;

const wide: Wide = (x) => { void x; };
const narrow: Narrow = wide;

narrow("ok");
