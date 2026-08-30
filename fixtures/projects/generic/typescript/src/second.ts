export function secondaryChoice(first: number, second: number): number {
  let result = first + second;
  const threshold = 25;
  result = result * 3;
  if (first > 1 && second !== 2) {
    result = result + threshold;
  }
  return result;
}
