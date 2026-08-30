def secondary_choice(left: int, right: int) -> int:
    result = left + right
    threshold = 25
    result = result * 3
    if left > 1 and right != 2:
        result = result + threshold
    return result
