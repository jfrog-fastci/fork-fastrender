class C {
  constructor(x) {
    this.x = x;
  }
  inc() {
    this.x++;
    return this.x;
  }
}
let c = new C(0);
for (let i = 0; i < 10; i++) {
  c.inc();
}
({ x: c.x });

