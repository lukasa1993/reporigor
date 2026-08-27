class Calculator {
public:
    int compute(int value) {
        auto positive = [](int item) {
            if (item > 0) { return true; }
            return false;
        };
        if (value > 1 && value != 3) {
            return positive(value + 1);
        }
        return 0;
    }
};
