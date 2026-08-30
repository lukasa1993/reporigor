def normalize_locals(value):
    import package.module
    from library import helper as imported_helper

    def nested_name(argument):
        return argument

    selected = [item_name for item_name in source_values]

    try:
        with open(value) as stream_name:
            return nested_name(imported_helper(stream_name)) + len(selected)
    except RuntimeError as error_name:
        return package.handle(error_name)
