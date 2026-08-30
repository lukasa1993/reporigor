class PrimaryChoice {
public:
  int choose(int left, int right) const {
    int total = left + right;
    int limit = 10;
    total = total * 2;
    if (left > 0 && right != 0) {
      total = total + limit;
    }
    return total;
  }
};
