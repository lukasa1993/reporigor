export function primaryChoice(left: number, right: number): number {
  let total = left + right;
  const limit = 10;
  total = total * 2;
  if (left > 0 && right !== 0) {
    total = total + limit;
  }
  return total;
}
