def scoped(values):
    before = item()
    transform = lambda lambda_value: nested_dependency(lambda_value) if guard(lambda_value) else fallback(lambda_value)
    selected = [item for item in values if predicate(item)]
    return before + len(selected)
