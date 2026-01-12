// @lib: es5
// @moduleResolution: node

// @filename: main.ts
import { value } from "dep";

export const from_dep = value;

// @filename: node_modules/dep/index.ts
export const value: string = "dep";
