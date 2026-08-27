int classify_c(int left, int right) {
  if (left > 0 && right > 0) {
    return left + right;
  }
  return 0;
}
