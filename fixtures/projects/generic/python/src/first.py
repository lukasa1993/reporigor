def primary_choice(left: int, right: int) -> int:
    total = left + right
    limit = 10
    total = total * 2
    if left > 0 and right != 0:
        total = total + limit
    return total
