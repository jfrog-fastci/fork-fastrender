declare function unknown_cond(): boolean;
declare function side_effect_true(): void;
declare function side_effect_false(): void;
declare function unknown_func(x: number): void;

let x = 0;
if (unknown_cond()) {
  side_effect_true();
  x = 1;
} else {
  side_effect_false();
  x = 2;
}
unknown_func(x);

