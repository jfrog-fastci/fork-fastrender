// @lib: es5
// @filename: a.ts
export const a = 1;

// @filename: b.ts
export { a } from "./a";

// @filename: c.ts
import { a } from "./b";
const n: number = a;
void n;
