// Proxy traps (get/set/ownKeys) + function apply/construct.
const target = { a: 1 };
const p = new Proxy(target, {
  get(t, prop, recv) {
    if (prop === "b") return 2;
    return Reflect.get(t, prop, recv);
  },
  set(t, prop, value, recv) {
    return Reflect.set(t, prop, value, recv);
  },
  ownKeys(t) {
    return Reflect.ownKeys(t).concat(["b"]);
  },
  getOwnPropertyDescriptor(t, prop) {
    if (prop === "b") {
      return { configurable: true, enumerable: true, writable: true, value: 2 };
    }
    return Reflect.getOwnPropertyDescriptor(t, prop);
  },
});

p.a;
p.b;
p.c = 3;
Object.keys(p);

const fn = new Proxy(function (x) { return x + 1; }, {
  apply(t, thisArg, args) {
    return Reflect.apply(t, thisArg, args);
  },
  construct(t, args, newTarget) {
    return Reflect.construct(t, args, newTarget);
  },
});

fn(1);
new fn(2);

