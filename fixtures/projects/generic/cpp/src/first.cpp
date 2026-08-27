class PrimaryChoice {
public:
  int choose(int left, int right) const {
    if (left > 0 && right != 0) {
      return left + right;
    }
    return 0;
  }
};
