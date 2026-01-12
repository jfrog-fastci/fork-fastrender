// @lib: es5

// @filename: a.ts
export const a: number = 1;

// @filename: b.ts
export { a } from "./a";

// @filename: main.ts
import { a } from "./b";

export const useA: number = a;
