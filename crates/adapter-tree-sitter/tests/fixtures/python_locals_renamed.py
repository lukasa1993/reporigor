def normalize_locals(value):
    import package.module
    from library import helper as alternate_helper

    def alternate_nested(argument):
        return argument + 1

    alternatives = [alternate_item for alternate_item in source_values]

    try:
        with open(value) as alternate_stream:
            return alternate_nested(alternate_helper(alternate_stream)) + len(alternatives)
    except RuntimeError as alternate_error:
        return package.handle(alternate_error)
