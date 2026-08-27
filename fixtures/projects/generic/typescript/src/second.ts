export function secondaryChoice(left: number, right: number): number {
  if (left > 0 && right !== 0) {
    return left + right;
  }
  return 0;
}
