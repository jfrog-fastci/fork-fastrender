// @lib: es5

// @filename: main.ts
/// <reference path="./node_modules/dep.ts" />

import { depValue } from "ambient";

export const x: number = depValue;

// @filename: node_modules/dep.ts
declare module "ambient" {
  export const depValue: number;
}
