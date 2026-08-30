// Moving a declaration without changing its structure must not change symbols.


class Converter {
public:
    int convert(int candidate) {
        int placeholder, reserve;
        int temporary = candidate + 99;
        if (temporary > 0) {
            while (temporary > 1) {
                temporary--;
            }
        }
        return helper(temporary);
    }

    double convert(double input) {
        double result = input + 12.5;
        return helper_double(result);
    }
};
