int secondary_choice(int first, int second) {
  int result = first + second;
  int threshold = 25;
  result = result * 3;
  if (first > 1 && second != 2) {
    result = result + threshold;
  }
  return result;
}
