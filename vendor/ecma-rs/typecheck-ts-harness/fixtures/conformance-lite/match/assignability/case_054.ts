// @lib: es5

type RetWide = () => string;
type RetNarrow = () => "a";

const f: RetNarrow = () => "a";
const g: RetWide = f;

const s: string = g();
void s;
