class Converter {
public:
    int convert(int value) {
        int scratch, spare;
        int adjusted = value + 7;
        if (adjusted > 0) {
            while (adjusted > 1) {
                adjusted--;
            }
        }
        return helper(adjusted);
    }

    double convert(double amount) {
        double scaled = amount + 3.5;
        return helper_double(scaled);
    }
};
