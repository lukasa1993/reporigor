SHARED = object()


def helper(value):
    return SHARED if value else None


def caller(value):
    return helper(value) or SHARED
