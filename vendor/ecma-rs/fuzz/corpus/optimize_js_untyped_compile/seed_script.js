let x = 0;
function bump(n) {
  return n + 1;
}
if (x) {
  x = bump(x);
} else {
  x = bump(41);
}
x;

