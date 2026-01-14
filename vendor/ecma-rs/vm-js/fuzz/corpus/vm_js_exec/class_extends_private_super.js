// Classes + extends/super + private fields/methods.
class Base {
  constructor(x) {
    this.x = x;
  }
  inc() {
    this.x++;
    return this.x;
  }
}

class Derived extends Base {
  #y = 1;
  constructor(x) {
    super(x);
    this.#y = x + 1;
  }
  sum() {
    return super.inc() + this.#y;
  }
  static #s = 1;
  static bump() {
    return ++this.#s;
  }
}

new Derived(2).sum();
Derived.bump();

