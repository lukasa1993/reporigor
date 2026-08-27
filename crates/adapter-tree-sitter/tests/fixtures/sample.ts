class Thing {
    choose(value: number): boolean {
        if (value > 1 && value != 3) {
            return true;
        }
        return false;
    }
}

const positive = (value: number): boolean => value > 0 ? true : false;
