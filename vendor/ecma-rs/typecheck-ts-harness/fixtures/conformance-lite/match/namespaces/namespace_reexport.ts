// @lib: es5
// @filename: a.ts
export namespace N {
  export const x = 1;
}

// @filename: b.ts
export { N } from "./a";

// @filename: c.ts
import { N } from "./b";
const n: number = N.x;
void n;
