def scoped(values):
    previous = item()
    alternate_transform = lambda renamed_value: deep_dependency(renamed_value) if first_guard(renamed_value) and second_guard(renamed_value) else alternate_fallback(renamed_value)
    choices = [element for element in values if predicate(element)]
    return previous + len(choices)
