class Choice {
public:
  int choose(bool a, bool b) const {
    if (a && b) { return 1; }
    return 0;
  }
};
