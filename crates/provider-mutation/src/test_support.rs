pub(crate) trait RequiredFixture<T> {
    fn required(self) -> T;
}

impl<T, E: std::fmt::Debug> RequiredFixture<T> for Result<T, E> {
    fn required(self) -> T {
        match self {
            Ok(value) => value,
            Err(error) => panic!("provider fixture operation failed: {error:?}"),
        }
    }
}

impl<T> RequiredFixture<T> for Option<T> {
    fn required(self) -> T {
        match self {
            Some(value) => value,
            None => panic!("provider fixture value must be present"),
        }
    }
}

pub(crate) fn must<T>(value: impl RequiredFixture<T>) -> T {
    value.required()
}
