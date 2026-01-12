// @filename: main.ts
// @moduleResolution: node10
export const resolved: Promise<number> = Promise.resolve(1);

// @filename: node_modules/pkg/index.d.ts
export const fromPkg: string;

// @filename: use_pkg.ts
import { fromPkg } from "pkg";
export const pkgValue: string = fromPkg;
