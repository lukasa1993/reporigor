int primary_choice(int left, int right) {
  int total = left + right;
  int limit = 10;
  total = total * 2;
  if (left > 0 && right != 0) {
    total = total + limit;
  }
  return total;
}
