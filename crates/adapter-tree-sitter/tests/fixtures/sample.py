def choose(value):
    label = "true == false"
    # true == false must not become mutation candidates.
    if value > 1 and value != 3:
        return value + len(label)
    return 0
