// @lib: es5
// @moduleResolution: node

// @filename: main.ts
import { value } from "dep";

export const from_dep = value;

// @filename: node_modules/dep/index.ts
/// <reference lib="es2015.promise" />

export const value: Promise<number> = Promise.resolve(1);
